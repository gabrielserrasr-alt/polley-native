-- IMPORTANTE: o diagnóstico mostrou que a tabela messages estava SEM o trigger guard_message
-- (a 0002 não tinha sido aplicada). Esta migration recria a função E liga o trigger na tabela,
-- então ela sozinha já deixa tudo certo.
--
-- Anti-spam mais firme: no máximo 5 mensagens a cada 5 segundos por pessoa (a mesma regra do
-- Discord). O limite de 10 em 10 s da 0006 era folgado demais: apertando Enter no ritmo de uma
-- pessoa, nunca chegava a 10 dentro da janela.
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
  new.created_at := now(); -- ninguém escolhe a data da própria mensagem
  new.edited_at := null;
  return new;
end $$;

-- Liga a função na tabela (sem isto, nada acima roda).
drop trigger if exists guard_message on public.messages;
create trigger guard_message before insert or update on public.messages
  for each row execute function public.guard_message();

-- Conferência: deve mostrar antispam = true e o trigger ligado.
select
  (select prosrc like '%5 seconds%' from pg_proc where proname = 'guard_message') as antispam,
  (select string_agg(tgname || ' (ligado=' || tgenabled::text || ')', ', ')
     from pg_trigger where tgrelid = 'public.messages'::regclass and not tgisinternal) as triggers_de_messages,
  (select string_agg(tgname, ', ') from pg_trigger where tgrelid = 'public.profiles'::regclass and not tgisinternal) as triggers_de_profiles,
  (select string_agg(tgname, ', ') from pg_trigger where tgrelid = 'public.servers'::regclass and not tgisinternal) as triggers_de_servers;
