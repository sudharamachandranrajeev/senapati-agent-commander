// Senapati WhatsApp sidecar: a linked-device WhatsApp client (Baileys) driven over JSON lines.
// WARNING: unofficial client. WhatsApp may ban the linked number. Link a spare number only.
//
//   node index.mjs --auth <dir> --link   show a QR in this terminal, exit once linked
//   node index.mjs --auth <dir>          stdin:  {"op":"send","to":"<digits>","text":"..."}
//                                        stdout: {"event":"ready"|"qr"|"closed"|"logged_out"|"error"|"message",...}

import makeWASocket, { useMultiFileAuthState, DisconnectReason, fetchLatestBaileysVersion } from 'baileys'
import pino from 'pino'
import qrcode from 'qrcode-terminal'
import readline from 'node:readline'

const args = process.argv.slice(2)
const authIdx = args.indexOf('--auth')
const authDir = authIdx >= 0 ? args[authIdx + 1] : undefined
const linkMode = args.includes('--link')
if (!authDir) {
  console.error('usage: node index.mjs --auth <dir> [--link]')
  process.exit(64)
}

const emit = (o) => { if (!linkMode) process.stdout.write(JSON.stringify(o) + '\n') }
const say = (s) => { if (linkMode) console.log(s) }

let sock
let open = false

async function connect () {
  const { state, saveCreds } = await useMultiFileAuthState(authDir)
  // An outdated protocol version gets rejected; fall back to the built-in one if the lookup fails.
  const { version } = await fetchLatestBaileysVersion().catch(() => ({ version: undefined }))
  sock = makeWASocket({
    auth: state,
    version,
    logger: pino({ level: 'silent' }),
    browser: ['Senapati', 'Desktop', '1.0'],
    markOnlineOnConnect: false,
    syncFullHistory: false
  })
  sock.ev.on('creds.update', saveCreds)

  sock.ev.on('connection.update', (u) => {
    if (u.qr) {
      if (linkMode) {
        qrcode.generate(u.qr, { small: true })
        say('On the BOT phone: WhatsApp > Settings > Linked devices > Link a device, then scan.')
      } else {
        emit({ event: 'qr' })
      }
    }
    if (u.connection === 'open') {
      open = true
      emit({ event: 'ready' })
      if (linkMode) {
        say(`Linked. Session saved in ${authDir}. Keep that folder private.`)
        setTimeout(() => process.exit(0), 3000)
      }
    }
    if (u.connection === 'close') {
      open = false
      const code = u.lastDisconnect?.error?.output?.statusCode
      if (code === DisconnectReason.loggedOut) {
        emit({ event: 'logged_out' })
        say('Logged out. Delete the auth folder and link again.')
        process.exit(2)
      }
      emit({ event: 'closed', code })
      setTimeout(() => connect().catch(fatal), code === DisconnectReason.restartRequired ? 0 : 5000)
    }
  })

  sock.ev.on('messages.upsert', ({ messages, type }) => {
    if (type !== 'notify') return
    for (const m of messages) {
      if (m.key.fromMe) continue
      // Newer chats address people by LID; the phone-number JID is in the alt fields.
      const jid = [m.key.remoteJid, m.key.remoteJidAlt, m.key.senderPn]
        .find((j) => typeof j === 'string' && j.endsWith('@s.whatsapp.net'))
      if (!jid) continue
      const text = m.message?.conversation ?? m.message?.extendedTextMessage?.text
      if (!text) continue
      emit({ event: 'message', from: jid.split('@')[0].split(':')[0], text })
    }
  })
}

function fatal (e) {
  emit({ event: 'error', message: String(e?.message ?? e) })
  say(String(e?.stack ?? e))
  process.exit(1)
}

if (!linkMode) {
  const rl = readline.createInterface({ input: process.stdin })
  rl.on('line', async (line) => {
    let msg
    try { msg = JSON.parse(line) } catch { return }
    if (msg.op !== 'send') return
    if (typeof msg.to !== 'string' || !/^\d{6,15}$/.test(msg.to) || typeof msg.text !== 'string') {
      emit({ event: 'error', message: 'bad send request' })
      return
    }
    if (!open) {
      emit({ event: 'error', message: 'not connected; message dropped' })
      return
    }
    try {
      await sock.sendMessage(`${msg.to}@s.whatsapp.net`, { text: msg.text })
    } catch (e) {
      emit({ event: 'error', message: `send failed: ${e?.message ?? e}` })
    }
  })
  // Parent exited: stop instead of lingering as an orphan.
  rl.on('close', () => process.exit(0))
}

connect().catch(fatal)
