// Edge Function: exclui a conta de quem chamou (LGPD).
// Deploy: painel do Supabase → Edge Functions → nova função "delete-account" → colar este código.
// Não precisa de secret novo: SUPABASE_URL, SUPABASE_ANON_KEY e SUPABASE_SERVICE_ROLE_KEY
// já existem dentro das Edge Functions (a service_role NUNCA sai do servidor).
//
// 1. Confere o token (quem está pedindo) e a SENHA de novo — um token roubado sozinho não apaga a conta.
// 2. Apaga os arquivos da pessoa no Storage (fotos, banners, anexos: tudo na pasta <uid>/).
// 3. Apaga o usuário do Auth. O banco apaga o resto em cascata: perfil, amizades, DMs,
//    mensagens e os servidores dos quais ela é dona.
import { createClient } from 'npm:@supabase/supabase-js@2'

const cors = {
  'Access-Control-Allow-Origin': '*',
  'Access-Control-Allow-Headers': 'authorization, x-client-info, apikey, content-type'
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { ...cors, 'Content-Type': 'application/json' } })

const URL = Deno.env.get('SUPABASE_URL')!
const ANON = Deno.env.get('SUPABASE_ANON_KEY')!
const SERVICE = Deno.env.get('SUPABASE_SERVICE_ROLE_KEY')!
const BUCKETS = ['avatars', 'banners', 'attachments']

Deno.serve(async (req) => {
  if (req.method === 'OPTIONS') return new Response('ok', { headers: cors })
  if (req.method !== 'POST') return json({ error: 'método não permitido' }, 405)

  const authHeader = req.headers.get('Authorization')
  if (!authHeader) return json({ error: 'sem autorização' }, 401)

  const asUser = createClient(URL, ANON, { global: { headers: { Authorization: authHeader } } })
  const { data: userData, error: userErr } = await asUser.auth.getUser()
  if (userErr || !userData.user?.email) return json({ error: 'sessão inválida' }, 401)
  const user = userData.user

  const { password } = (await req.json().catch(() => ({}))) as { password?: string }
  if (!password) return json({ error: 'Digite sua senha para confirmar.' }, 400)

  // Confere a senha num cliente separado (não guarda sessão).
  const check = createClient(URL, ANON, { auth: { persistSession: false, autoRefreshToken: false } })
  const { data: signed, error: pwErr } = await check.auth.signInWithPassword({ email: user.email!, password })
  if (pwErr || signed.user?.id !== user.id) return json({ error: 'Senha incorreta.' }, 403)

  const admin = createClient(URL, SERVICE, { auth: { persistSession: false, autoRefreshToken: false } })

  // Arquivos: lista a pasta <uid>/ de cada bucket (de 1000 em 1000) e apaga.
  for (const bucket of BUCKETS) {
    for (;;) {
      const { data: files, error } = await admin.storage.from(bucket).list(user.id, { limit: 1000 })
      if (error) return json({ error: 'não consegui apagar seus arquivos, tente de novo' }, 500)
      if (!files || files.length === 0) break
      const paths = files.filter((f) => f.id).map((f) => `${user.id}/${f.name}`)
      if (paths.length === 0) break
      const { error: rmErr } = await admin.storage.from(bucket).remove(paths)
      if (rmErr) return json({ error: 'não consegui apagar seus arquivos, tente de novo' }, 500)
      if (files.length < 1000) break
    }
  }

  const { error: delErr } = await admin.auth.admin.deleteUser(user.id)
  if (delErr) return json({ error: 'não consegui excluir a conta, tente de novo' }, 500)

  return json({ ok: true })
})
