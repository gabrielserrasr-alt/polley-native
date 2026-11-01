-- =====================================================================
-- 0009 — Anexos privados
--
-- Antes o bucket "attachments" era público: quem tivesse o link de um arquivo mandado numa
-- DM (ou num servidor) conseguia abrir, mesmo sem estar na conversa. Agora:
--   * o bucket é privado;
--   * cada anexo fica registrado em message_files (caminho do arquivo → mensagem);
--   * só baixa quem pode VER a mensagem (as mesmas regras de ler mensagens) ou quem enviou.
-- O app (0.2.7+) baixa os anexos com o login da pessoa em vez do link público.
-- Rodar no SQL Editor do Supabase. No fim, tudo deve dar true.
-- =====================================================================

-- Arquivo → mensagem em que ele aparece. Ninguém lê essa tabela direto (sem policies);
-- quem usa é a função can_read_attachment, abaixo.
create table if not exists public.message_files (
  path       text not null,
  message_id uuid not null references public.messages on delete cascade,
  primary key (path, message_id)
);
alter table public.message_files enable row level security;
revoke all on public.message_files from anon, authenticated;

-- Caminho dentro do bucket a partir da URL gravada na mensagem (o guard_message já conferiu
-- que ela é do nosso Storage e da pasta de quem enviou).
create or replace function public.attachment_path(url text) returns text
language sql immutable as $$
  select substring(url from '/storage/v1/object/public/attachments/([A-Za-z0-9._/-]+)$')
$$;

-- Mensagem nova com anexos: registra cada arquivo.
create or replace function public.index_message_files() returns trigger
language plpgsql security definer set search_path = public as $$
declare
  a jsonb;
  p text;
begin
  for a in select * from jsonb_array_elements(new.attachments) loop
    p := public.attachment_path(a->>'url');
    if p is not null then
      insert into public.message_files (path, message_id) values (p, new.id) on conflict do nothing;
    end if;
  end loop;
  return new;
end $$;

drop trigger if exists index_message_files on public.messages;
create trigger index_message_files after insert on public.messages
  for each row execute function public.index_message_files();

-- Anexos que já existem.
insert into public.message_files (path, message_id)
select public.attachment_path(a->>'url'), m.id
from public.messages m, jsonb_array_elements(m.attachments) a
where public.attachment_path(a->>'url') is not null
on conflict do nothing;

-- Pode baixar o arquivo? = consegue ver alguma mensagem que tem esse anexo.
-- (can_access_message_target usa o auth.uid() de quem está pedindo.)
create or replace function public.can_read_attachment(p text) returns boolean
language sql stable security definer set search_path = public as $$
  select exists (
    select 1
    from public.message_files f
    join public.messages m on m.id = f.message_id
    where f.path = p and public.can_access_message_target(m.channel_id, m.dm_id)
  )
$$;
revoke execute on function public.can_read_attachment(text) from public, anon;
grant execute on function public.can_read_attachment(text) to authenticated;

-- Bucket privado + regra de leitura.
update storage.buckets set public = false where id = 'attachments';

drop policy if exists "anexos: quem vê a mensagem" on storage.objects;
create policy "anexos: quem vê a mensagem" on storage.objects
  for select to authenticated
  using (bucket_id = 'attachments' and (owner = auth.uid() or public.can_read_attachment(name)));

-- ---------------------------------------------------------------------
-- CONFERÊNCIA: tudo deve dar true.
-- ---------------------------------------------------------------------
select
  (select not public from storage.buckets where id = 'attachments') as bucket_privado,
  exists (select 1 from pg_tables where schemaname = 'public' and tablename = 'message_files') as tabela_arquivos,
  exists (select 1 from pg_trigger where tgname = 'index_message_files') as registra_anexos,
  exists (select 1 from pg_policies where schemaname = 'storage' and tablename = 'objects'
          and policyname = 'anexos: quem vê a mensagem') as regra_leitura,
  not exists (
    select 1 from public.messages m, jsonb_array_elements(m.attachments) a
    where public.attachment_path(a->>'url') is not null
      and not exists (select 1 from public.message_files f
                      where f.message_id = m.id and f.path = public.attachment_path(a->>'url'))
  ) as anexos_antigos_registrados;
