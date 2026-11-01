-- Revisão de segurança geral (passo 7).
-- Tudo aqui é "defesa no banco": vale mesmo se alguém usar a API sem o app.

-- ---------------------------------------------------------------------
-- 1) BLOQUEIO VALE DE VERDADE
--    Antes: bloquear só escondia a pessoa no app. Se já existia uma DM, quem foi bloqueado
--    continuava conseguindo mandar mensagens e "ligar" pela API.
-- ---------------------------------------------------------------------
create or replace function public.is_blocked_between(a uuid, b uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (
    select 1 from friendships
    where status = 'blocked'
      and least(requester_id, addressee_id) = least(a, b)
      and greatest(requester_id, addressee_id) = greatest(a, b)
  )
$$;

-- Nesta DM, eu e a outra pessoa temos um bloqueio (de qualquer lado)?
create or replace function public.dm_blocked(did uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (
    select 1 from dm_members outro
    where outro.dm_id = did and outro.user_id <> auth.uid()
      and public.is_blocked_between(auth.uid(), outro.user_id)
  )
$$;

drop policy if exists "envia mensagens onde participa" on public.messages;
create policy "envia mensagens onde participa" on public.messages
  for insert to authenticated
  with check (
    author_id = auth.uid()
    and public.can_access_message_target(channel_id, dm_id)
    and (dm_id is null or not public.dm_blocked(dm_id))
  );

drop policy if exists "ring: só quem tem DM comigo me liga" on realtime.messages;
create policy "ring: só quem tem DM comigo me liga" on realtime.messages
  for insert to authenticated
  with check (
    realtime.messages.extension = 'broadcast'
    and realtime.topic() like 'ring:%'
    and exists (
      select 1
      from public.dm_members eu
      join public.dm_members outro on outro.dm_id = eu.dm_id
      where eu.user_id = auth.uid()
        and outro.user_id::text = substr(realtime.topic(), 6)
        and outro.user_id <> auth.uid()
        and not public.is_blocked_between(auth.uid(), outro.user_id)
    )
  );

-- ---------------------------------------------------------------------
-- 2) PERFIL COM LIMITES
--    Antes: nome de exibição e tema sem limite de tamanho (alguém podia colocar megabytes e
--    travar o app de todo mundo que visse o perfil), "membro desde" editável e foto/banner
--    apontando para qualquer lugar. NOT VALID = não confere perfis antigos, só os novos/editados.
-- ---------------------------------------------------------------------
alter table public.profiles drop constraint if exists profiles_display_name_len;
alter table public.profiles add constraint profiles_display_name_len
  check (display_name is null or char_length(display_name) between 1 and 32) not valid;

alter table public.profiles drop constraint if exists profiles_theme_size;
alter table public.profiles add constraint profiles_theme_size
  check (jsonb_typeof(theme) = 'object' and pg_column_size(theme) <= 2048) not valid;

alter table public.profiles drop constraint if exists profiles_images_ours;
alter table public.profiles add constraint profiles_images_ours check (
  (avatar_url is null or avatar_url ~ ('^https://[a-z0-9]+\.supabase\.co/storage/v1/object/public/avatars/' || id::text || '/[A-Za-z0-9._-]+$'))
  and
  (banner_url is null or banner_url ~ ('^https://[a-z0-9]+\.supabase\.co/storage/v1/object/public/banners/' || id::text || '/[A-Za-z0-9._-]+$'))
) not valid;

create or replace function public.guard_profile() returns trigger
language plpgsql as $$
begin
  new.id := old.id;
  new.created_at := old.created_at;
  return new;
end $$;
drop trigger if exists guard_profile on public.profiles;
create trigger guard_profile before update on public.profiles
  for each row execute function public.guard_profile();

-- ---------------------------------------------------------------------
-- 3) ANTI-SPAM: no máximo 10 mensagens a cada 10 segundos por pessoa.
--    (Mesmo trigger de 0002, com a conta de mensagens recentes no INSERT.)
-- ---------------------------------------------------------------------
create index if not exists messages_author_idx on public.messages (author_id, created_at desc);

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
       or new.attachments is distinct from old.attachments then
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
      where author_id = new.author_id and created_at > now() - interval '10 seconds') >= 10 then
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
  new.created_at := now(); -- ninguém escolhe a data da própria mensagem
  new.edited_at := null;
  return new;
end $$;

-- ---------------------------------------------------------------------
-- 4) SERVIDORES
--    Convites novos com 12 caracteres (antes 8 = dava para "chutar" códigos por força bruta).
--    O dono só muda nome/ícone direto; dono e convite não mudam por UPDATE solto.
-- ---------------------------------------------------------------------
alter table public.servers alter column invite_code
  set default substr(encode(gen_random_bytes(8), 'hex'), 1, 12);

create or replace function public.guard_server() returns trigger
language plpgsql as $$
begin
  new.id := old.id;
  new.owner_id := old.owner_id;
  new.invite_code := old.invite_code;
  new.created_at := old.created_at;
  return new;
end $$;
drop trigger if exists guard_server on public.servers;
create trigger guard_server before update on public.servers
  for each row execute function public.guard_server();

-- ---------------------------------------------------------------------
-- 5) STORAGE COM LIMITES
--    Foto e banner: só imagem, até 5 MB. Anexos: até 25 MB (o mesmo limite do app).
-- ---------------------------------------------------------------------
update storage.buckets
  set file_size_limit = 5242880,
      allowed_mime_types = array['image/png', 'image/jpeg', 'image/webp', 'image/gif']
  where id in ('avatars', 'banners');
update storage.buckets set file_size_limit = 26214400 where id = 'attachments';
