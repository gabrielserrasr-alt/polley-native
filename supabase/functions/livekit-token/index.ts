// Edge Function: gera token do LiveKit para quem tem acesso ao canal de voz ou à DM,
// e aplica a moderação de voz dos servidores.
// Deploy:  supabase functions deploy livekit-token
// Secrets: LIVEKIT_API_KEY, LIVEKIT_API_SECRET e LIVEKIT_URL (o mesmo endereço wss:// do app)
//
// Ações (campo "action" no corpo):
//   (nenhuma)   → token para entrar na chamada (channelId ou dmId)
//   "moderate"  → moderador silencia/desativa o áudio de alguém no servidor (vale na hora)
//   "remove"    → tira da chamada quem acabou de ser expulso/banido
import { createClient } from 'npm:@supabase/supabase-js@2'
import { AccessToken, RoomServiceClient, TrackSource } from 'npm:livekit-server-sdk@2'

const cors = {
  'Access-Control-Allow-Origin': '*',
  'Access-Control-Allow-Headers': 'authorization, x-client-info, apikey, content-type'
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { ...cors, 'Content-Type': 'application/json' } })

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

// Sem microfone: pode câmera e tela, mas não voz.
const SEM_MICROFONE = ['camera', 'screen_share', 'screen_share_audio']
const SEM_MICROFONE_ENUM = [TrackSource.CAMERA, TrackSource.SCREEN_SHARE, TrackSource.SCREEN_SHARE_AUDIO]

function roomService() {
  const url = Deno.env.get('LIVEKIT_URL')
  if (!url) throw new Error('falta o secret LIVEKIT_URL na Edge Function')
  return new RoomServiceClient(
    url.replace(/^wss:/, 'https:').replace(/^ws:/, 'http:'),
    Deno.env.get('LIVEKIT_API_KEY')!,
    Deno.env.get('LIVEKIT_API_SECRET')!
  )
}

Deno.serve(async (req) => {
  if (req.method === 'OPTIONS') return new Response('ok', { headers: cors })

  const authHeader = req.headers.get('Authorization')
  if (!authHeader) return json({ error: 'sem autorização' }, 401)

  // Cliente com o JWT do usuário: as consultas abaixo passam pela RLS,
  // então "conseguir ler o canal" == "ser membro do servidor".
  const supabase = createClient(Deno.env.get('SUPABASE_URL')!, Deno.env.get('SUPABASE_ANON_KEY')!, {
    global: { headers: { Authorization: authHeader } }
  })

  const { data: userData, error: userErr } = await supabase.auth.getUser()
  if (userErr || !userData.user) return json({ error: 'sessão inválida' }, 401)
  const user = userData.user

  const body = (await req.json()) as {
    action?: string
    channelId?: string
    dmId?: string
    serverId?: string
    userId?: string
    muted?: boolean
    deafened?: boolean
  }

  // ------------------------------------------------ moderação de voz
  if (body.action === 'moderate' || body.action === 'remove') {
    const { serverId, userId } = body
    if (!serverId || !userId || !uuid.test(serverId) || !uuid.test(userId)) return json({ error: 'pedido inválido' }, 400)

    if (body.action === 'moderate') {
      // A função do banco confere se quem pediu pode moderar essa pessoa.
      const { error } = await supabase.rpc('set_voice_moderation', {
        p_server: serverId,
        p_user: userId,
        p_muted: !!body.muted,
        p_deafened: !!body.deafened
      })
      if (error) return json({ error: error.message.includes('permissão') ? 'sem permissão' : error.message }, 403)
    } else {
      // Tirar da chamada: só dono/admin do servidor.
      const { data: admin } = await supabase.rpc('is_server_admin', { sid: serverId })
      if (!admin) return json({ error: 'sem permissão' }, 403)
    }

    // Aplica em todas as salas de voz do servidor (a pessoa só pode estar em uma).
    const { data: channels } = await supabase.from('channels').select('id').eq('server_id', serverId).eq('type', 'voice')
    let svc: RoomServiceClient
    try {
      svc = roomService()
    } catch (e) {
      return json({ error: String((e as Error).message) }, 500)
    }
    const semVoz = !!body.muted || !!body.deafened
    for (const c of channels ?? []) {
      const room = `ch:${c.id}`
      try {
        if (body.action === 'remove') {
          await svc.removeParticipant(room, userId)
        } else {
          // Tirar a permissão do microfone faz o LiveKit desligar o que ela estava publicando.
          await svc.updateParticipant(room, userId, undefined, {
            canPublish: true,
            canSubscribe: true,
            canPublishData: true,
            canPublishSources: semVoz ? SEM_MICROFONE_ENUM : []
          })
        }
      } catch {
        // Não está nesta sala: normal.
      }
    }
    return json({ ok: true })
  }

  // ------------------------------------------------ token para entrar
  const { channelId, dmId } = body
  let room: string
  let semMicrofone = false

  if (channelId) {
    const { data } = await supabase.from('channels').select('id,type,server_id').eq('id', channelId).maybeSingle()
    if (!data || data.type !== 'voice') return json({ error: 'sem acesso ao canal' }, 403)
    room = `ch:${channelId}`
    // Silenciado por um moderador: entra sem poder publicar microfone.
    const { data: member } = await supabase
      .from('server_members')
      .select('voice_muted,voice_deafened')
      .eq('server_id', data.server_id)
      .eq('user_id', user.id)
      .maybeSingle()
    semMicrofone = !!member?.voice_muted || !!member?.voice_deafened
  } else if (dmId) {
    const { data } = await supabase.from('dm_channels').select('id').eq('id', dmId).maybeSingle()
    if (!data) return json({ error: 'sem acesso à DM' }, 403)
    room = `dm:${dmId}`
  } else {
    return json({ error: 'informe channelId ou dmId' }, 400)
  }

  const { data: profile } = await supabase
    .from('profiles')
    .select('username,display_name,avatar_url')
    .eq('id', user.id)
    .single()

  const at = new AccessToken(Deno.env.get('LIVEKIT_API_KEY')!, Deno.env.get('LIVEKIT_API_SECRET')!, {
    identity: user.id,
    name: profile?.display_name || profile?.username || 'user',
    metadata: JSON.stringify({ avatar_url: profile?.avatar_url ?? null }),
    ttl: '6h'
  })
  at.addGrant({
    room,
    roomJoin: true,
    canPublish: true,
    canSubscribe: true,
    canPublishData: true,
    ...(semMicrofone ? { canPublishSources: SEM_MICROFONE } : {})
  })

  return json({ token: await at.toJwt(), room })
})
