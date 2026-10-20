// Experiment only: direct TLS to a literal-loopback fixture, with an SPKI pin
// obtained from the Navidrome radio announcement. No authentication or audio.
// Trust here is an exact advertised key, not the system CA store. The socket is
// never used for HTTP until its TLS handshake completes AND the pin matches.
import { X509Certificate, createHash, timingSafeEqual } from 'node:crypto'
import { appendFileSync, readFileSync, writeFileSync } from 'node:fs'
import https from 'node:https'
import tls from 'node:tls'

if (process.argv[2] === 'serve') {
  const [key, cert, installation, ready, requests] = process.argv.slice(3)
  const server = https.createServer({ key: readFileSync(key), cert: readFileSync(cert),
    minVersion: 'TLSv1.3' }, (req, res) => {
    appendFileSync(requests, JSON.stringify({ path: req.url, has_auth:
      ['authorization', 'cookie', 'x-nd-authorization'].some(name => req.headers[name] !== undefined) }) + '\n')
    res.writeHead(req.url === '/proxy/stratum/access/v1/capabilities' ? 200 : 404,
      { 'Content-Type': 'application/json', 'Connection': 'close' })
    res.end(JSON.stringify({ protocol: 'stratum-radio-experiment', version: 1, installation }))
  })
  const timer = setTimeout(() => process.exit(2), 90000)
  server.on('tlsClientError', () => {})
  server.listen(0, '127.0.0.1', () => {
    writeFileSync(ready, JSON.stringify({ endpoint:
      'https://127.0.0.1:' + server.address().port + '/proxy/stratum/access/v1/capabilities' }))
  })
  process.once('SIGTERM', () => {
    clearTimeout(timer)
    server.close(() => process.exit(0))
    setTimeout(() => process.exit(0), 1000).unref()
  })
} else {
  let input = ''
  for await (const chunk of process.stdin) {
    input += chunk
    if (input.length > 8192) throw new Error('Input too large')
  }
  const { endpoint, spkiSha256 } = JSON.parse(input)
  const url = new URL(endpoint)
  if (url.protocol !== 'https:' || url.hostname !== '127.0.0.1' || !url.port ||
      url.username || url.password || url.search || url.hash ||
      url.pathname !== '/proxy/stratum/access/v1/capabilities' ||
      !/^[a-f0-9]{64}$/.test(spkiSha256)) throw new Error('Invalid fixture endpoint or pin')

  const result = await new Promise(resolve => {
    let done = false
    let matched = false
    let bytes = Buffer.alloc(0)
    const socket = tls.connect({ host: url.hostname, port: Number(url.port),
      minVersion: 'TLSv1.3', rejectUnauthorized: false })
    const timer = setTimeout(() => finish({ ok: false, reason: 'timeout' }), 3000)
    function finish(value) {
      if (done) return
      done = true
      clearTimeout(timer)
      socket.destroy()
      resolve({ pinMatched: matched, ...value })
    }
    socket.once('secureConnect', () => {
      try {
        const peer = new X509Certificate(socket.getPeerCertificate().raw)
        const key = peer.publicKey.export({ type: 'spki', format: 'der' })
        const actual = createHash('sha256').update(key).digest()
        if (!timingSafeEqual(actual, Buffer.from(spkiSha256, 'hex'))) {
          finish({ ok: false, reason: 'identity-mismatch' })
          return
        }
        matched = true
        // Fixed public metadata only. Never send Navidrome auth to discovery.
        socket.write(`GET ${url.pathname} HTTP/1.0\r\nHost: ${url.host}\r\nConnection: close\r\n\r\n`)
      } catch {
        finish({ ok: false, reason: 'identity-invalid' })
      }
    })
    socket.on('data', chunk => {
      if (bytes.length + chunk.length > 8192) {
        finish({ ok: false, reason: 'response-too-large' })
        return
      }
      bytes = Buffer.concat([bytes, chunk])
    })
    socket.once('end', () => {
      try {
        const split = bytes.indexOf('\r\n\r\n')
        if (!matched || split < 0 || !/^HTTP\/1\.[01] 200 /.test(bytes.toString('ascii', 0, split))) {
          throw new Error('Unexpected fixture response')
        }
        finish({ ok: true, info: JSON.parse(bytes.subarray(split + 4).toString('utf8')) })
      } catch {
        finish({ ok: false, reason: 'invalid-response' })
      }
    })
    socket.once('error', () => finish({ ok: false, reason: 'connection-failed' }))
    socket.once('close', () => finish({ ok: false, reason: 'connection-closed' }))
  })
  process.stdout.write(JSON.stringify(result) + '\n')
}
