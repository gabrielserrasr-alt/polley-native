-- 0008: moderação de voz, respostas, reações e configurações do servidor.
-- Cole TUDO no SQL Editor do Supabase e rode. No final aparece uma linha de conferência:
-- todas as colunas devem mostrar "true".

-- ---------------------------------------------------------------------
-- 1) SILENCIAR VOZ / DESATIVAR ÁUDIO NO SERVIDOR (moderadores)
--    Só muda pela função abaixo (não existe policy de UPDATE em server_members).
--    A Edge Function livekit-token usa isto para tirar o microfone da pessoa no LiveKit.
-- ---------------------------------------------------------------------
alter table public.server_members
  add column if not exists voice_muted boolean not null default false,
  add column if not exists voice_deafened boolean not null default false;

create or replace function public.set_voice_moderation(p_server uuid, p_user uuid, p_muted boolean, p_deafened boolean)
returns void
language plpgsql security definer set search_path = public as $$
begin
  if not public.can_moderate(p_server, p_user) then raise exception 'sem permissão'; end if;
  update server_members set voice_muted = p_muted, voice_deafened = p_deafened
    where server_id = p_server and user_id = p_user;
  if not found then raise exception 'não é membro'; end if;
end $$;

revoke execute on function public.set_voice_moderation(uuid, uuid, boolean, boolean) from public, anon;
grant execute on function public.set_voice_moderation(uuid, uuid, boolean, boolean) to authenticated;

-- ---------------------------------------------------------------------
-- 2) RESPONDER MENSAGEM
--    reply_to aponta para outra mensagem da MESMA conversa (senão vira null).
-- ---------------------------------------------------------------------
alter table public.messages
  add column if not exists reply_to uuid references public.messages(id) on delete set null;

create or replace function public.guard_message() returns trigger
language plpgsql as $$
declare
  a jsonb;
begin
  if tg_op = 'UPDATE' then
    if new.channel_id is distinct from old.channel_id
       or new.dm_id is distinct from old.dm_id
       or new.author_id is distinct from old.author_id
       or new.created_at is distinct from old.created_at
       or new.attachments is distinct from old.attachments
       -- a resposta só pode sumir (quando a mensagem original é apagada)
       or (new.reply_to is distinct from old.reply_to and new.reply_to is not null) then
      raise exception 'só dá para editar o texto da mensagem';
    end if;
    if new.content is distinct from old.content then
      new.edited_at := now();
    else
      new.edited_at := old.edited_at;
    end if;
    return new;
  end if;

  -- INSERT
  if (select count(*) from public.messages
      where author_id = new.author_id and created_at > now() - interval '5 seconds') >= 5 then
    raise exception 'Calma! Muitas mensagens seguidas. Espere alguns segundos.';
  end if;
  if jsonb_typeof(new.attachments) <> 'array' or jsonb_array_length(new.attachments) > 10 then
    raise exception 'anexos inválidos';
  end if;
  for a in select * from jsonb_array_elements(new.attachments) loop
    if coalesce(a->>'url', '') !~ ('^https://[a-z0-9]+\.supabase\.co/storage/v1/object/public/attachments/'
                                    || new.author_id::text || '/[A-Za-z0-9._-]+$') then
      raise exception 'anexo fora do Storage do Polley';
    end if;
  end loop;
  if new.reply_to is not null and not exists (
    select 1 from public.messages r
    where r.id = new.reply_to
      and r.channel_id is not distinct from new.channel_id
      and r.dm_id is not distinct from new.dm_id
  ) then
    new.reply_to := null; -- resposta a mensagem de outra conversa: ignora
  end if;
  new.created_at := now(); -- ninguém escolhe a data da própria mensagem
  new.edited_at := null;
  return new;
end $$;

drop trigger if exists guard_message on public.messages;
create trigger guard_message before insert or update on public.messages
  for each row execute function public.guard_message();

-- ---------------------------------------------------------------------
-- 3) REAÇÕES
--    Uma linha por (mensagem, pessoa, emoji). O canal/DM é copiado da mensagem para o app
--    assinar só as reações da conversa aberta.
-- ---------------------------------------------------------------------
create table if not exists public.message_reactions (
  message_id uuid not null references public.messages(id) on delete cascade,
  user_id    uuid not null default auth.uid() references public.profiles(id) on delete cascade,
  emoji      text not null check (char_length(emoji) between 1 and 16),
  channel_id uuid,
  dm_id      uuid,
  created_at timestamptz not null default now(),
  primary key (message_id, user_id, emoji)
);
create index if not exists message_reactions_channel_idx on public.message_reactions (channel_id);
create index if not exists message_reactions_dm_idx on public.message_reactions (dm_id);

create or replace function public.guard_reaction() returns trigger
language plpgsql security definer set search_path = public as $$
begin
  new.user_id := auth.uid();
  new.created_at := now();
  select m.channel_id, m.dm_id into new.channel_id, new.dm_id from messages m where m.id = new.message_id;
  if (select count(distinct emoji) from message_reactions
      where message_id = new.message_id and emoji <> new.emoji) >= 20 then
    raise exception 'no máximo 20 reações diferentes por mensagem';
  end if;
  if (select count(*) from message_reactions
      where user_id = new.user_id and created_at > now() - interval '10 seconds') >= 15 then
    raise exception 'Calma! Muitas reações seguidas.';
  end if;
  return new;
end $$;
drop trigger if exists guard_reaction on public.message_reactions;
create trigger guard_reaction before insert on public.message_reactions
  for each row execute function public.guard_reaction();

alter table public.message_reactions enable row level security;
drop policy if exists "vê reações onde participa" on public.message_reactions;
create policy "vê reações onde participa" on public.message_reactions
  for select to authenticated using (public.can_access_message_target(channel_id, dm_id));
drop policy if exists "reage onde participa" on public.message_reactions;
create policy "reage onde participa" on public.message_reactions
  for insert to authenticated
  with check (
    user_id = auth.uid()
    and public.can_access_message_target(channel_id, dm_id)
    and (dm_id is null or not public.dm_blocked(dm_id))
  );
drop policy if exists "tira a própria reação" on public.message_reactions;
create policy "tira a própria reação" on public.message_reactions
  for delete to authenticated using (user_id = auth.uid());

alter table public.message_reactions replica identity full;
do $$ begin
  alter publication supabase_realtime add table public.message_reactions;
exception when duplicate_object then null;
end $$;

-- ---------------------------------------------------------------------
-- 4) CONFIGURAÇÕES DO SERVIDOR
--    Ícone só do nosso Storage (pasta do dono). Convite novo só pela função new_invite.
-- ---------------------------------------------------------------------
alter table public.servers drop constraint if exists servers_icon_storage;
alter table public.servers add constraint servers_icon_storage check (
  icon_url is null or icon_url ~ ('^https://[a-z0-9]+\.supabase\.co/storage/v1/object/public/avatars/'
                                  || owner_id::text || '/[A-Za-z0-9._-]+$')
) not valid;
alter table public.servers drop constraint if exists servers_name_size;
alter table public.servers add constraint servers_name_size check (char_length(trim(name)) between 1 and 100) not valid;

create or replace function public.guard_server() returns trigger
language plpgsql as $$
begin
  new.id := old.id;
  new.owner_id := old.owner_id;
  new.created_at := old.created_at;
  -- O convite só muda dentro da função new_invite (ela liga esta marca só durante o UPDATE).
  if coalesce(current_setting('polley.new_invite', true), '') <> '1' then
    new.invite_code := old.invite_code;
  end if;
  return new;
end $$;
drop trigger if exists guard_server on public.servers;
create trigger guard_server before update on public.servers
  for each row execute function public.guard_server();

create or replace function public.new_invite(p_server uuid) returns text
language plpgsql security definer set search_path = public, extensions as $$
declare
  code text;
begin
  if not public.is_server_admin(p_server) then raise exception 'sem permissão'; end if;
  code := substr(encode(gen_random_bytes(8), 'hex'), 1, 12);
  perform set_config('polley.new_invite', '1', true);
  update servers set invite_code = code where id = p_server;
  perform set_config('polley.new_invite', '', true);
  return code;
end $$;
revoke execute on function public.new_invite(uuid) from public, anon;
grant execute on function public.new_invite(uuid) to authenticated;

-- ---------------------------------------------------------------------
-- CONFERÊNCIA: tudo deve dar true.
-- ---------------------------------------------------------------------
select
  exists (select 1 from information_schema.columns
          where table_schema = 'public' and table_name = 'server_members' and column_name = 'voice_muted') as moderacao_voz,
  exists (select 1 from pg_proc where proname = 'set_voice_moderation') as funcao_moderacao,
  exists (select 1 from information_schema.columns
          where table_schema = 'public' and table_name = 'messages' and column_name = 'reply_to') as respostas,
  (select prosrc like '%reply_to%' from pg_proc where proname = 'guard_message') as guarda_respostas,
  exists (select 1 from pg_tables where schemaname = 'public' and tablename = 'message_reactions') as reacoes,
  exists (select 1 from pg_trigger where tgname = 'guard_reaction') as guarda_reacoes,
  exists (select 1 from pg_publication_tables
          where pubname = 'supabase_realtime' and tablename = 'message_reactions') as reacoes_ao_vivo,
  exists (select 1 from pg_proc where proname = 'new_invite') as novo_convite,
  exists (select 1 from pg_trigger where tgname = 'guard_server') as guarda_servidor;
