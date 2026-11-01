-- Atualização automática do Polley nativo.
-- Bucket PÚBLICO só para leitura: o app baixa o latest.json e o instalador daqui.
-- Não criamos nenhuma policy de INSERT/UPDATE/DELETE, então nenhum usuário do app consegue
-- enviar ou trocar arquivos — só você, pelo painel do Supabase.
-- (E mesmo que alguém trocasse, o app recusa o que não tiver a assinatura da sua chave.)
insert into storage.buckets (id, name, public, file_size_limit)
values ('releases', 'releases', true, 52428800) -- 50 MB por arquivo
on conflict (id) do update set public = true, file_size_limit = 52428800;
