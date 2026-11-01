-- "Tocar o telefone" de uma ligação pela DM, por um canal PRIVADO do Realtime: ring:<id do usuário>.
--
-- Por que privado: a presença ("online") é vista por todo mundo. Se a ligação passasse por ela,
-- qualquer um saberia quem está ligando para quem. Aqui:
--   - só EU consigo escutar o meu canal ring:<meu id>;
--   - só quem tem uma DM comigo consegue mandar um toque para ele.

create policy "ring: escuto só o meu" on realtime.messages
  for select to authenticated
  using (
    realtime.messages.extension = 'broadcast'
    and realtime.topic() = 'ring:' || auth.uid()::text
  );

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
    )
  );
