-- =====================================================================
-- Polley — schema inicial
-- Rode no Supabase: SQL Editor → cole tudo → Run  (ou `supabase db push`)
-- =====================================================================

create extension if not exists pgcrypto;

-- ---------------------------------------------------------------------
-- PERFIS
-- ---------------------------------------------------------------------
create table public.profiles (
  id           uuid primary key references auth.users on delete cascade,
  username     text not null unique check (username ~ '^[a-z0-9_.]{2,32}$'),
  display_name text,
  bio          text check (char_length(bio) <= 190),
  avatar_url   text,
  banner_url   text,
  theme        jsonb not null default '{"primary":"#5865f2","accent":"#eb459e","background":"#232428","text":"#f2f3f5"}',
  created_at   timestamptz not null default now()
);

alter table public.profiles enable row level security;
create policy "perfis visíveis para logados" on public.profiles
  for select to authenticated using (true);
create policy "edita o próprio perfil" on public.profiles
  for update to authenticated using (id = auth.uid()) with check (id = auth.uid());

-- Cria o perfil automaticamente no cadastro (username vem do signUp options.data)
create function public.handle_new_user() returns trigger
language plpgsql security definer set search_path = public as $$
begin
  insert into public.profiles (id, username, display_name)
  values (
    new.id,
    lower(coalesce(new.raw_user_meta_data->>'username', 'user_' || substr(new.id::text, 1, 8))),
    coalesce(new.raw_user_meta_data->>'display_name', new.raw_user_meta_data->>'username')
  );
  return new;
end $$;

create trigger on_auth_user_created after insert on auth.users
  for each row execute function public.handle_new_user();

-- ---------------------------------------------------------------------
-- AMIZADES
-- ---------------------------------------------------------------------
create table public.friendships (
  id           uuid primary key default gen_random_uuid(),
  requester_id uuid not null references public.profiles on delete cascade,
  addressee_id uuid not null references public.profiles on delete cascade,
  status       text not null default 'pending' check (status in ('pending','accepted','blocked')),
  created_at   timestamptz not null default now(),
  check (requester_id <> addressee_id)
);
-- Um único registro por par, independente de quem pediu
create unique index friendships_pair on public.friendships
  (least(requester_id, addressee_id), greatest(requester_id, addressee_id));

alter table public.friendships enable row level security;
create policy "vê as próprias amizades" on public.friendships
  for select to authenticated using (auth.uid() in (requester_id, addressee_id));
-- Pedir amizade ou bloquear: o autor é sempre o requester
create policy "cria pedido ou bloqueio" on public.friendships
  for insert to authenticated
  with check (requester_id = auth.uid() and status in ('pending','blocked'));
-- Só quem recebeu pode aceitar
create policy "aceita pedido" on public.friendships
  for update to authenticated
  using (addressee_id = auth.uid() and status = 'pending')
  with check (status = 'accepted');
-- Qualquer lado desfaz amizade/pedido; bloqueio só quem bloqueou desfaz
create policy "remove amizade" on public.friendships
  for delete to authenticated
  using (
    (status <> 'blocked' and auth.uid() in (requester_id, addressee_id))
    or (status = 'blocked' and requester_id = auth.uid())
  );

-- ---------------------------------------------------------------------
-- SERVIDORES, MEMBROS, CANAIS
-- ---------------------------------------------------------------------
create table public.servers (
  id          uuid primary key default gen_random_uuid(),
  name        text not null check (char_length(name) between 1 and 100),
  icon_url    text,
  owner_id    uuid not null references public.profiles on delete cascade,
  invite_code text not null unique default substr(encode(gen_random_bytes(6), 'hex'), 1, 8),
  created_at  timestamptz not null default now()
);

create table public.server_members (
  server_id uuid references public.servers on delete cascade,
  user_id   uuid references public.profiles on delete cascade,
  role      text not null default 'member' check (role in ('owner','admin','member')),
  joined_at timestamptz not null default now(),
  primary key (server_id, user_id)
);

create table public.channels (
  id         uuid primary key default gen_random_uuid(),
  server_id  uuid not null references public.servers on delete cascade,
  name       text not null check (char_length(name) between 1 and 100),
  type       text not null default 'text' check (type in ('text','voice')),
  position   int not null default 0,
  created_at timestamptz not null default now()
);

-- Helpers SECURITY DEFINER evitam recursão infinita de RLS
-- (a policy de server_members precisaria consultar server_members).
create function public.is_server_member(sid uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (select 1 from server_members where server_id = sid and user_id = auth.uid())
$$;

create function public.is_server_admin(sid uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (select 1 from server_members
                 where server_id = sid and user_id = auth.uid() and role in ('owner','admin'))
$$;

create function public.can_access_channel(cid uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (select 1 from channels c join server_members m on m.server_id = c.server_id
                 where c.id = cid and m.user_id = auth.uid())
$$;

alter table public.servers enable row level security;
create policy "membros veem o servidor" on public.servers
  for select to authenticated using (public.is_server_member(id));
create policy "dono edita" on public.servers
  for update to authenticated using (owner_id = auth.uid());
create policy "dono apaga" on public.servers
  for delete to authenticated using (owner_id = auth.uid());

alter table public.server_members enable row level security;
create policy "membros veem membros" on public.server_members
  for select to authenticated using (public.is_server_member(server_id));
create policy "sair ou expulsar" on public.server_members
  for delete to authenticated
  using ((user_id = auth.uid() and role <> 'owner') or public.is_server_admin(server_id));

alter table public.channels enable row level security;
create policy "membros veem canais" on public.channels
  for select to authenticated using (public.is_server_member(server_id));
create policy "admins criam canais" on public.channels
  for insert to authenticated with check (public.is_server_admin(server_id));
create policy "admins editam canais" on public.channels
  for update to authenticated using (public.is_server_admin(server_id));
create policy "admins apagam canais" on public.channels
  for delete to authenticated using (public.is_server_admin(server_id));

-- Criar servidor = servidor + dono como membro + canais padrão, tudo atômico
create function public.create_server(p_name text) returns uuid
language plpgsql security definer set search_path = public as $$
declare sid uuid;
begin
  if auth.uid() is null then raise exception 'não autenticado'; end if;
  insert into servers (name, owner_id) values (p_name, auth.uid()) returning id into sid;
  insert into server_members (server_id, user_id, role) values (sid, auth.uid(), 'owner');
  insert into channels (server_id, name, type, position) values
    (sid, 'geral', 'text', 0),
    (sid, 'Geral', 'voice', 1);
  return sid;
end $$;

-- Entrar por código de convite (o convidado ainda não "vê" o servidor pela RLS)
create function public.join_server(p_code text) returns uuid
language plpgsql security definer set search_path = public as $$
declare sid uuid;
begin
  if auth.uid() is null then raise exception 'não autenticado'; end if;
  select id into sid from servers where invite_code = lower(trim(p_code));
  if sid is null then raise exception 'convite inválido'; end if;
  insert into server_members (server_id, user_id) values (sid, auth.uid())
    on conflict do nothing;
  return sid;
end $$;

-- ---------------------------------------------------------------------
-- DMs
-- ---------------------------------------------------------------------
create table public.dm_channels (
  id         uuid primary key default gen_random_uuid(),
  created_at timestamptz not null default now()
);

create table public.dm_members (
  dm_id   uuid references public.dm_channels on delete cascade,
  user_id uuid references public.profiles on delete cascade,
  primary key (dm_id, user_id)
);

create function public.is_dm_member(did uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (select 1 from dm_members where dm_id = did and user_id = auth.uid())
$$;

alter table public.dm_channels enable row level security;
create policy "participantes veem a DM" on public.dm_channels
  for select to authenticated using (public.is_dm_member(id));

alter table public.dm_members enable row level security;
create policy "participantes veem participantes" on public.dm_members
  for select to authenticated using (public.is_dm_member(dm_id));

-- Abre (ou reaproveita) a DM com um amigo
create function public.get_or_create_dm(p_other uuid) returns uuid
language plpgsql security definer set search_path = public as $$
declare did uuid;
begin
  if auth.uid() is null then raise exception 'não autenticado'; end if;
  if not exists (
    select 1 from friendships
    where status = 'accepted'
      and least(requester_id, addressee_id) = least(auth.uid(), p_other)
      and greatest(requester_id, addressee_id) = greatest(auth.uid(), p_other)
  ) then raise exception 'vocês precisam ser amigos'; end if;

  select a.dm_id into did from dm_members a
    join dm_members b on b.dm_id = a.dm_id
    where a.user_id = auth.uid() and b.user_id = p_other
    limit 1;
  if did is not null then return did; end if;

  insert into dm_channels default values returning id into did;
  insert into dm_members (dm_id, user_id) values (did, auth.uid()), (did, p_other);
  return did;
end $$;

-- ---------------------------------------------------------------------
-- MENSAGENS
-- ---------------------------------------------------------------------
create table public.messages (
  id          uuid primary key default gen_random_uuid(),
  channel_id  uuid references public.channels on delete cascade,
  dm_id       uuid references public.dm_channels on delete cascade,
  author_id   uuid not null default auth.uid() references public.profiles on delete cascade,
  content     text not null default '' check (char_length(content) <= 4000),
  attachments jsonb not null default '[]',
  created_at  timestamptz not null default now(),
  edited_at   timestamptz,
  check ((channel_id is null) <> (dm_id is null))
);
create index messages_channel_idx on public.messages (channel_id, created_at desc);
create index messages_dm_idx on public.messages (dm_id, created_at desc);

create function public.can_access_message_target(cid uuid, did uuid) returns boolean
language sql stable as $$
  select (cid is not null and public.can_access_channel(cid))
      or (did is not null and public.is_dm_member(did))
$$;

alter table public.messages enable row level security;
create policy "lê mensagens onde participa" on public.messages
  for select to authenticated using (public.can_access_message_target(channel_id, dm_id));
create policy "envia mensagens onde participa" on public.messages
  for insert to authenticated
  with check (author_id = auth.uid() and public.can_access_message_target(channel_id, dm_id));
create policy "edita as próprias" on public.messages
  for update to authenticated using (author_id = auth.uid()) with check (author_id = auth.uid());
create policy "apaga as próprias ou é admin" on public.messages
  for delete to authenticated
  using (
    author_id = auth.uid()
    or (channel_id is not null and exists (
      select 1 from public.channels c where c.id = channel_id and public.is_server_admin(c.server_id)))
  );

-- ---------------------------------------------------------------------
-- REALTIME
-- ---------------------------------------------------------------------
alter publication supabase_realtime add table
  public.messages, public.friendships, public.server_members, public.channels, public.profiles;

-- ---------------------------------------------------------------------
-- STORAGE (avatares, banners, anexos) — cada usuário escreve só na pasta <uid>/
-- ---------------------------------------------------------------------
insert into storage.buckets (id, name, public) values
  ('avatars', 'avatars', true),
  ('banners', 'banners', true),
  ('attachments', 'attachments', true)
on conflict (id) do nothing;

create policy "upload na própria pasta" on storage.objects
  for insert to authenticated
  with check (bucket_id in ('avatars','banners','attachments')
              and (storage.foldername(name))[1] = auth.uid()::text);
create policy "atualiza os próprios arquivos" on storage.objects
  for update to authenticated
  using (bucket_id in ('avatars','banners','attachments') and owner = auth.uid());
create policy "apaga os próprios arquivos" on storage.objects
  for delete to authenticated
  using (bucket_id in ('avatars','banners','attachments') and owner = auth.uid());
