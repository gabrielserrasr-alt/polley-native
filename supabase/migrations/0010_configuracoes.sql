-- =====================================================================
-- 0010 — Configurações (igual ao Discord)
--
-- 1) Privacidade: quem pode te mandar pedido de amizade (todos / amigos de amigos /
--    membros dos meus servidores). Conferido NO BANCO: vale mesmo usando a API sem o app.
-- 2) Desativar conta: a conta some para pedidos novos até a pessoa entrar de novo.
-- 3) Verificação em 2 etapas valendo de verdade: quem ativou precisa do código do app
--    autenticador (sessão "aal2"). Só a senha (sessão "aal1") não lê nem escreve nada.
-- Rodar no SQL Editor do Supabase. No fim, tudo deve dar true.
-- =====================================================================

-- ---------------------------------------------------------------------
-- 1) PRIVACIDADE DE PEDIDOS DE AMIZADE
-- ---------------------------------------------------------------------
create table if not exists public.user_privacy (
  user_id            uuid primary key references public.profiles on delete cascade default auth.uid(),
  fr_everyone        boolean not null default true,
  fr_friends_of_friends boolean not null default true,
  fr_server_members  boolean not null default true
);
alter table public.user_privacy enable row level security;
drop policy if exists "privacidade: só a própria" on public.user_privacy;
create policy "privacidade: só a própria" on public.user_privacy
  for all to authenticated
  using (user_id = auth.uid())
  with check (user_id = auth.uid());

alter table public.profiles add column if not exists deactivated boolean not null default false;

-- Pode `a` mandar pedido de amizade para `b`?
create or replace function public.can_send_friend_request(a uuid, b uuid) returns boolean
language sql stable security definer set search_path = public as $$
  select
    not coalesce((select deactivated from profiles where id = b), false)
    and (
      coalesce((select fr_everyone from user_privacy where user_id = b), true)
      -- amigos de amigos: alguém é amigo dos dois
      or (coalesce((select fr_friends_of_friends from user_privacy where user_id = b), true) and exists (
        select 1
        from friendships f1
        join friendships f2
          on (case when f2.requester_id = b then f2.addressee_id else f2.requester_id end)
           = (case when f1.requester_id = a then f1.addressee_id else f1.requester_id end)
        where f1.status = 'accepted' and a in (f1.requester_id, f1.addressee_id)
          and f2.status = 'accepted' and b in (f2.requester_id, f2.addressee_id)
      ))
      -- membros de um servidor em comum
      or (coalesce((select fr_server_members from user_privacy where user_id = b), true) and exists (
        select 1 from server_members m1
        join server_members m2 on m2.server_id = m1.server_id
        where m1.user_id = a and m2.user_id = b
      ))
    )
$$;

create or replace function public.guard_friend_request() returns trigger
language plpgsql security definer set search_path = public as $$
begin
  if new.status = 'pending' and not public.can_send_friend_request(new.requester_id, new.addressee_id) then
    raise exception 'Essa pessoa não está aceitando pedidos de amizade de você.';
  end if;
  return new;
end $$;
drop trigger if exists guard_friend_request on public.friendships;
create trigger guard_friend_request before insert on public.friendships
  for each row execute function public.guard_friend_request();

-- ---------------------------------------------------------------------
-- 3) VERIFICAÇÃO EM 2 ETAPAS
--    Regra "restritiva": soma-se às outras regras de cada tabela. Quem tem 2 etapas ativada
--    só passa com a sessão confirmada pelo código (aal2).
-- ---------------------------------------------------------------------
create or replace function public.mfa_ok() returns boolean
language sql stable security definer set search_path = public, auth as $$
  select coalesce(auth.jwt() ->> 'aal', 'aal1') = 'aal2'
      or not exists (select 1 from auth.mfa_factors f where f.user_id = auth.uid() and f.status = 'verified')
$$;
revoke execute on function public.mfa_ok() from public, anon;
grant execute on function public.mfa_ok() to authenticated;

do $$
declare
  t record;
begin
  -- Todas as tabelas do app (as que têm RLS ligada).
  for t in select schemaname, tablename from pg_tables where schemaname = 'public' and rowsecurity loop
    execute format('drop policy if exists "2 etapas" on %I.%I', t.schemaname, t.tablename);
    execute format(
      'create policy "2 etapas" on %I.%I as restrictive for all to authenticated
         using ((select public.mfa_ok())) with check ((select public.mfa_ok()))',
      t.schemaname, t.tablename);
  end loop;
end $$;

-- Arquivos (anexos, fotos) e o tempo real (ligações, digitando, presença).
drop policy if exists "2 etapas" on storage.objects;
create policy "2 etapas" on storage.objects as restrictive for all to authenticated
  using ((select public.mfa_ok())) with check ((select public.mfa_ok()));
drop policy if exists "2 etapas" on realtime.messages;
create policy "2 etapas" on realtime.messages as restrictive for all to authenticated
  using ((select public.mfa_ok())) with check ((select public.mfa_ok()));

-- ---------------------------------------------------------------------
-- CONFERÊNCIA: tudo deve dar true.
-- ---------------------------------------------------------------------
select
  exists (select 1 from pg_tables where schemaname = 'public' and tablename = 'user_privacy') as privacidade,
  exists (select 1 from information_schema.columns
          where table_schema = 'public' and table_name = 'profiles' and column_name = 'deactivated') as desativar_conta,
  exists (select 1 from pg_trigger where tgname = 'guard_friend_request') as regra_pedidos,
  exists (select 1 from pg_proc where proname = 'mfa_ok') as funcao_2_etapas,
  not exists (
    select 1 from pg_tables t
    where t.schemaname = 'public' and t.rowsecurity
      and not exists (select 1 from pg_policies p
                      where p.schemaname = 'public' and p.tablename = t.tablename and p.policyname = '2 etapas')
  ) as todas_tabelas_com_2_etapas,
  exists (select 1 from pg_policies where schemaname = 'storage' and tablename = 'objects' and policyname = '2 etapas') as arquivos_2_etapas,
  exists (select 1 from pg_policies where schemaname = 'realtime' and tablename = 'messages' and policyname = '2 etapas') as tempo_real_2_etapas;
