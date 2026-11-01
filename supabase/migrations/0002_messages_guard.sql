-- Proteções extras nas mensagens.
--
-- 1) Editar só o texto. A regra "edita as próprias" deixava o autor trocar QUALQUER coluna,
--    inclusive channel_id/dm_id: dava para "mover" uma mensagem para um canal ou DM em que
--    a pessoa nem está. Agora só content (e edited_at, preenchido pelo banco) podem mudar.
-- 2) Anexos só do nosso Storage, na pasta de quem enviou (no máximo 10). Sem isso, um anexo
--    poderia apontar para qualquer site e o app de todo mundo baixaria de lá.

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
  if jsonb_typeof(new.attachments) <> 'array' or jsonb_array_length(new.attachments) > 10 then
    raise exception 'anexos inválidos';
  end if;
  for a in select * from jsonb_array_elements(new.attachments) loop
    if coalesce(a->>'url', '') !~ ('^https://[a-z0-9]+\.supabase\.co/storage/v1/object/public/attachments/'
                                    || new.author_id::text || '/[A-Za-z0-9._-]+$') then
      raise exception 'anexo fora do Storage do Polley';
    end if;
  end loop;
  new.edited_at := null;
  return new;
end $$;

drop trigger if exists guard_message on public.messages;
create trigger guard_message before insert or update on public.messages
  for each row execute function public.guard_message();
