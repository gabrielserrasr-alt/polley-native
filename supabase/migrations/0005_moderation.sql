-- Moderação de servidores: expulsar, banir/desbanir e promover/remover admin.
--
-- Regras (as mesmas do Discord, simplificadas):
--   - o DONO pode tudo com qualquer membro (menos com ele mesmo);
--   - um ADMIN só expulsa/bane MEMBROS comuns (não mexe no dono nem em outros admins);
--   - só o DONO promove ou remove admins.
-- Tudo passa por funções (RPC) que conferem essas regras no servidor; o app não consegue
-- burlar mexendo direto nas tabelas.

-- 1) CORREÇÃO DE SEGURANÇA: antes, qualquer admin conseguia apagar QUALQUER linha de
--    server_members (inclusive a do dono). Agora o DELETE direto só serve para SAIR do servidor;
--    expulsar passa por kick_member/ban_member.
drop policy if exists "sair ou expulsar" on public.server_members;
create policy "sair do servidor" on public.server_members
  for delete to authenticated
  using (user_id = auth.uid() and role <> 'owner');

-- 2) Banidos: quem está aqui não entra de novo nem com o código de convite.
create table if not exists public.server_bans (
  server_id  uuid not null references public.servers on delete cascade,
  user_id    uuid not null references public.profiles on delete cascade,
  banned_by  uuid references public.profiles on delete set null,
  created_at timestamptz not null default now(),
  primary key (server_id, user_id)
);
alter table public.server_bans enable row level security;
-- Só admins/dono veem a lista (para desbanir). Inserir/apagar só pelas funções abaixo.
create policy "admins veem banidos" on public.server_bans
  for select to authenticated using (public.is_server_admin(server_id));

-- Papel de alguém num servidor (null = não é membro).
create or replace function public.member_role(sid uuid, uid uuid) returns text
language sql stable security definer set search_path = public as $$
  select role from server_members where server_id = sid and user_id = uid
$$;

-- Eu posso moderar `target` neste servidor? (dono: todos menos ele; admin: só membros comuns)
create or replace function public.can_moderate(sid uuid, target uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select target <> auth.uid() and case public.member_role(sid, auth.uid())
    when 'owner' then coalesce(public.member_role(sid, target), 'member') <> 'owner'
    when 'admin' then coalesce(public.member_role(sid, target), 'member') = 'member'
    else false
  end
$$;

-- Avisa a pessoa, no canal PRIVADO dela, que ela saiu do servidor (o app dela volta para o início).
create or replace function public.notify_removed(sid uuid, target uuid, how text) returns void
language plpgsql security definer set search_path = public as $$
begin
  perform realtime.send(jsonb_build_object('server_id', sid, 'how', how), 'removed', 'ring:' || target::text, true);
exception when others then
  null; -- o aviso é só uma gentileza: a expulsão vale mesmo se ele falhar
end $$;

create or replace function public.kick_member(p_server uuid, p_user uuid) returns void
language plpgsql security definer set search_path = public as $$
begin
  if not public.can_moderate(p_server, p_user) then raise exception 'sem permissão'; end if;
  if public.member_role(p_server, p_user) is null then raise exception 'não é membro'; end if;
  delete from server_members where server_id = p_server and user_id = p_user;
  perform public.notify_removed(p_server, p_user, 'kick');
end $$;

create or replace function public.ban_member(p_server uuid, p_user uuid) returns void
language plpgsql security definer set search_path = public as $$
begin
  if not public.can_moderate(p_server, p_user) then raise exception 'sem permissão'; end if;
  insert into server_bans (server_id, user_id, banned_by) values (p_server, p_user, auth.uid())
    on conflict do nothing;
  delete from server_members where server_id = p_server and user_id = p_user;
  perform public.notify_removed(p_server, p_user, 'ban');
end $$;

create or replace function public.unban_member(p_server uuid, p_user uuid) returns void
language plpgsql security definer set search_path = public as $$
begin
  if not public.is_server_admin(p_server) then raise exception 'sem permissão'; end if;
  delete from server_bans where server_id = p_server and user_id = p_user;
end $$;

-- Só o dono promove/rebaixa, e nunca o próprio dono.
create or replace function public.set_member_role(p_server uuid, p_user uuid, p_role text) returns void
language plpgsql security definer set search_path = public as $$
begin
  if p_role not in ('admin', 'member') then raise exception 'papel inválido'; end if;
  if public.member_role(p_server, auth.uid()) is distinct from 'owner' then raise exception 'sem permissão'; end if;
  if public.member_role(p_server, p_user) not in ('admin', 'member') then raise exception 'não é membro'; end if;
  update server_members set role = p_role where server_id = p_server and user_id = p_user;
end $$;

-- 3) Entrar por convite agora recusa quem foi banido.
create or replace function public.join_server(p_code text) returns uuid
language plpgsql security definer set search_path = public as $$
declare sid uuid;
begin
  if auth.uid() is null then raise exception 'não autenticado'; end if;
  select id into sid from servers where invite_code = lower(trim(p_code));
  if sid is null then raise exception 'convite inválido'; end if;
  if exists (select 1 from server_bans where server_id = sid and user_id = auth.uid()) then
    raise exception 'você foi banido deste servidor';
  end if;
  insert into server_members (server_id, user_id) values (sid, auth.uid())
    on conflict do nothing;
  return sid;
end $$;

-- Funções só para quem está logado.
revoke execute on function public.kick_member(uuid, uuid), public.ban_member(uuid, uuid),
  public.unban_member(uuid, uuid), public.set_member_role(uuid, uuid, text),
  public.notify_removed(uuid, uuid, text) from public, anon;
grant execute on function public.kick_member(uuid, uuid), public.ban_member(uuid, uuid),
  public.unban_member(uuid, uuid), public.set_member_role(uuid, uuid, text) to authenticated;
-- notify_removed é interna: ninguém chama direto.
revoke execute on function public.notify_removed(uuid, uuid, text) from authenticated;

-- A lista de membros atualiza ao vivo (entrou/saiu/virou admin).
alter table public.server_members replica identity full;
