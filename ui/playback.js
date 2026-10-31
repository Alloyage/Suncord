// Capture uses 8-bit NV12. Read its sequence header so WebCodecs gets the negotiated profile and level.
function streamVideoCodec(data, kind) {
  if (kind === 2) {
    for (let offset = 0; offset < data.length;) {
      const header = data[offset++], type = (header >> 3) & 15
      if (header & 4) offset++
      let size = data.length - offset
      if (header & 2) {
        size = 0
        let shift = 0, byte
        do {
          if (offset >= data.length || shift > 49) throw new Error('Invalid AV1 OBU size.')
          byte = data[offset++]
          size += (byte & 127) * 2 ** shift
          shift += 7
        } while (byte & 128)
      }
      const end = offset + size
      if (end > data.length) throw new Error('Truncated AV1 OBU.')
      if (type === 1) {
        let bit = offset * 8
        const read = count => {
          if (bit + count > end * 8) throw new Error('Truncated AV1 sequence header.')
          let value = 0
          while (count--) { value = value * 2 + ((data[bit >> 3] >> (7 - (bit & 7))) & 1); bit++ }
          return value
        }
        const profile = read(3)
        read(1)
        const reduced = read(1)
        if (!reduced) {
          if (read(1)) {
            read(32); read(32)
            if (read(1)) {
              let zeros = 0
              while (!read(1)) if (++zeros > 32) throw new Error('Invalid AV1 timing information.')
              read(zeros)
            }
            if (read(1)) { read(5); read(32); read(5); read(5) }
          }
          read(1); read(5); read(12)
        }
        const level = read(5), tier = !reduced && level > 7 && read(1) ? 'H' : 'M'
        return `av01.${profile}.${String(level).padStart(2, '0')}${tier}.08`
      }
      offset = end
    }
  } else {
    for (let i = 0; i + 7 < data.length; i++) {
      if (data[i] || data[i + 1] || data[i + 2] !== 1) continue
      if (kind === 0 && (data[i + 3] & 31) === 7)
        return 'avc1.' + [...data.subarray(i + 4, i + 7)].map(byte => byte.toString(16).padStart(2, '0')).join('')
      if (kind === 1 && ((data[i + 3] >> 1) & 63) === 33) {
        const header = []
        for (let j = i + 5; j < data.length && header.length < 13; j++) {
          if (data[j] === 3 && data[j - 1] === 0 && data[j - 2] === 0) continue
          header.push(data[j])
        }
        if (header.length < 13) break
        let flags = 0, compatibility = 0
        for (const byte of header.slice(2, 6)) flags = (flags << 8) | byte
        for (let bit = 0; bit < 32; bit++) { compatibility = (compatibility << 1) | (flags & 1); flags >>>= 1 }
        const constraints = header.slice(6, 12)
        while (constraints.length && !constraints.at(-1)) constraints.pop()
        const space = ['', 'A', 'B', 'C'][header[1] >> 6]
        return `hev1.${space}${header[1] & 31}.${(compatibility >>> 0).toString(16)}.${header[1] & 32 ? 'H' : 'L'}${header[12]}` +
          constraints.map(byte => '.' + byte.toString(16)).join('')
      }
    }
  }
  throw new Error('The received keyframe has no video configuration.')
}

// Encoded video and raw Opus arrive from the local Rust worker.
// Stream audio and video share the sender's RTCP clock and one AudioContext.
class BrowserPlayback {
  constructor(canvas, report, account, session) {
    this.account = account
    this.scope = { session, call: crypto.randomUUID() }
    this.canvas = canvas
    this.report = report
    this.audio = new AudioContext({ latencyHint: 'interactive', sampleRate: 48000 })
    // Every source plays through output, and each voice user through its own gain.
    this.output = this.audio.createGain()
    this.output.connect(this.audio.destination)
    this.streamOutput = this.audio.createGain()
    this.streamOutput.connect(this.output)
    this.volumes = new Map()
    this.speaker = null
    this.decoders = new Map()
    this.sources = new Map()
    this.pictures = []
    this.anchor = null
    this.closed = false
    this.streamFailed = false
    this.abort = new AbortController()
    this.videoDecoded = 0
    this.videoFrames = 0
    this.audioFrames = 0
  }
  async start() {
    if (!globalThis.AudioDecoder)
      throw new Error('This browser needs WebCodecs Opus decoding support.')
    await this.audio.resume()
    if (this.audio.state !== 'running')
      throw new Error('Allow browser audio playback and try again.')
    const opus = await AudioDecoder.isConfigSupported({
      codec: 'opus',
      sampleRate: 48000,
      numberOfChannels: 2,
    })
    if (!opus.supported) throw new Error('This browser cannot decode Opus audio.')
    const response = await fetch('/api/media', { signal: this.abort.signal, headers: {
      'X-Suncord-Account': this.account,
      'X-Suncord-Session': String(this.scope.session),
      'X-Suncord-Call': this.scope.call,
    } })
    if (!response.ok) throw new Error(await response.text())
    this.reader = response.body.getReader()
    this.read().catch((error) => {
      if (!this.closed) this.fail(error)
    })
    const paint = () => {
      if (this.closed) return
      this.pruneVoice()
      let picture
      while (
        this.pictures.length &&
        this.pictures[0].timestamp / 1e6 <= this.audio.currentTime
      ) {
        picture?.close()
        picture = this.pictures.shift()
      }
      if (picture) {
        if (this.canvas.width !== picture.displayWidth) this.canvas.width = picture.displayWidth
        if (this.canvas.height !== picture.displayHeight) this.canvas.height = picture.displayHeight
        this.canvas.getContext('2d', { alpha: false }).drawImage(picture, 0, 0)
        this.canvas.hidden = false
        picture.close()
        this.videoFrames++
      }
      this.animation = requestAnimationFrame(paint)
    }
    paint()
  }
  async read() {
    const header = new Uint8Array(4)
    let packet = null, filled = 0
    while (!this.closed) {
      const { value, done } = await this.reader.read()
      if (done) throw new Error('The local media connection closed.')
      let offset = 0
      while (offset < value.length) {
        const target = packet || header, count = Math.min(target.length - filled, value.length - offset)
        target.set(value.subarray(offset, offset + count), filled)
        filled += count; offset += count
        if (filled !== target.length) continue
        if (!packet) {
          const length = new DataView(header.buffer).getUint32(0)
          if (length < 22 || length > 1048598) throw new Error('Invalid local media frame.')
          packet = new Uint8Array(length)
          packet.set(header)
        } else {
          this.packet(packet)
          packet = null; filled = 0
        }
      }
    }
  }
  packet(bytes) {
    if (this.closed) return
    this.pruneVoice()
    const stream = bytes[4] === 2 || bytes[4] === 3
    if (stream && this.streamFailed) return
    try {
      this.decodePacket(bytes)
    } catch (error) {
      this.fail(error, stream)
    }
  }
  decodePacket(bytes) {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const kind = bytes[4]
    if (!kind || kind === 4) {
      this.reset(kind === 4)
      return
    }
    const user = view.getBigUint64(6).toString()
    if (kind === 5) {
      this.speaker = bytes[5] ? user : null
      return
    }
    const stream = kind !== 1,
      video = kind === 2
    const remote = view.getBigUint64(14)
    let time
    if (stream) {
      if (!this.anchor) this.anchor = { remote, local: this.audio.currentTime + (user === this.account ? 0.12 : 0.4) }
      time =
        this.anchor.local + Number(BigInt.asIntN(64, remote - this.anchor.remote)) / 4294967296
    }
    const id = stream ? (video ? 'video' : 'audio') : 'voice-' + user
    let entry = this.decoders.get(id)
    if (!entry) {
      if (this.decoders.size >= 66) throw new Error('Too many audio participants.')
      entry = {
        remote,
        local: this.audio.currentTime + 0.12,
        last: -1,
        seen: performance.now(),
        pending: [],
        times: [],
      }
      const active = () => !this.closed && this.decoders.get(id) === entry
      const error = (error) => {
        if (active()) this.fail(error, stream)
      }
      if (video) {
        if (!globalThis.VideoDecoder) throw new Error('This browser needs WebCodecs video decoding support to watch video.')
        if (!(bytes[5] & 1)) return
        const data = bytes.subarray(22)
        const codec = streamVideoCodec(data, bytes[5] >> 1)
        entry.decoder = new VideoDecoder({
          output: (frame) => {
            if (!active()) {
              frame.close()
              return
            }
            // Drawing can pause in an obscured tab while audio keeps advancing.
            while (
              this.pictures.length > 1 &&
              this.pictures[1].timestamp / 1e6 <= this.audio.currentTime
            ) this.pictures.shift().close()
            if (
              this.pictures.length >= 32 ||
              frame.timestamp / 1e6 > this.audio.currentTime + 2
            ) {
              frame.close()
              this.fail(
                new Error('Browser video playback fell behind. Watch the stream again to recover.'),
                true,
              )
              return
            }
            this.videoDecoded++
            this.pictures.push(frame)
          },
          error,
        })
        this.decoders.set(id, entry)
        entry.decoder.configure({ codec, optimizeForLatency: true })
      } else {
        if (!stream) {
          entry.user = user
          entry.gain = this.audio.createGain()
          entry.gain.gain.value = this.volumes.get(user) ?? 1
          entry.gain.connect(this.output)
        }
        entry.decoder = new AudioDecoder({
          output: (data) => {
            try {
              if (!active()) return
              // Chrome stamps decoded audio from the first chunk on and ignores later gaps.
              // Each chunk yields one output, so outputs take their chunk's time in order.
              const at = entry.times.shift()
              if (at === undefined || at < this.audio.currentTime) return
              if (at > this.audio.currentTime + 2 || this.sources.size >= 256)
                throw new Error('Browser audio exceeded the playback bound.')
              const buffer = this.audio.createBuffer(
                data.numberOfChannels,
                data.numberOfFrames,
                data.sampleRate,
              )
              for (let planeIndex = 0; planeIndex < data.numberOfChannels; planeIndex++)
                data.copyTo(buffer.getChannelData(planeIndex), {
                  planeIndex,
                  format: 'f32-planar',
                })
              const source = this.audio.createBufferSource()
              source.buffer = buffer
              source.connect(stream ? this.streamOutput : entry.gain)
              source.onended = () => {
                this.sources.delete(source)
                source.disconnect()
              }
              this.sources.set(source, stream)
              source.start(at)
              this.audioFrames++
            } catch (error) {
              this.fail(error, stream)
            } finally {
              data.close()
            }
          },
          error,
        })
        this.decoders.set(id, entry)
        entry.decoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 })
      }
    }
    const now = performance.now(), quiet = now - entry.seen > 250
    entry.seen = now
    if (!stream) {
      time = entry.local + Number(BigInt.asIntN(32, remote - entry.remote)) / 48000
      // Senders may pause their RTP clock between talk spurts, and it drifts from ours.
      // Each spurt, or a packet far off schedule, starts a fresh timeline.
      if (quiet || time < this.audio.currentTime - 0.2 || time > this.audio.currentTime + 0.3) {
        entry.remote = remote
        entry.local = time = this.audio.currentTime + 0.12
        entry.last = -1
        clearTimeout(entry.timer)
        entry.pending.length = 0
      }
    }
    if (time <= entry.last) {
      if (video) throw new Error('Received video clock moved backwards.')
      return
    }
    if (time > this.audio.currentTime + 2 || (video && time < this.audio.currentTime))
      throw new Error('Browser playback exceeded its latency bound.')
    if (time < this.audio.currentTime) return
    const init = {
      type: video && !(bytes[5] & 1) ? 'delta' : 'key',
      timestamp: Math.round(time * 1e6),
      data: bytes.subarray(22),
    }
    if (stream) {
      this.submit(entry, time, video ? new EncodedVideoChunk(init) : new EncodedAudioChunk(init))
    } else {
      if (entry.pending.some(frame => frame.time === time)) return
      if (entry.pending.length >= 32) throw new Error('Browser voice reorder queue exceeded its bound.')
      // Discord sends tiny DTX packets in silence, so only larger ones mark speech.
      if (bytes.length > 22 + 24) entry.voiced = Math.max(entry.voiced || 0, time + 0.02)
      // WebCodecs accepts ordered Opus chunks, but exposes no RTP FEC/PLC controls.
      const frame = { time, ready: performance.now() + 60, chunk: new EncodedAudioChunk(init) }
      const index = entry.pending.findIndex(frame => frame.time > time)
      entry.pending.splice(index < 0 ? entry.pending.length : index, 0, frame)
      this.flushVoice(id, entry)
    }
  }
  submit(entry, time, chunk) {
    if (entry.decoder.decodeQueueSize >= 32)
      throw new Error('Browser decoder fell behind.')
    entry.last = time
    if (chunk instanceof EncodedAudioChunk) entry.times.push(time)
    entry.decoder.decode(chunk)
  }
  flushVoice(id, entry) {
    clearTimeout(entry.timer)
    entry.timer = null
    if (this.closed || this.decoders.get(id) !== entry) return
    try {
      while (entry.pending.length) {
        const frame = entry.pending[0]
        // Leave 20 ms for decoding before the existing 120 ms playback anchor.
        const ready = Math.min(...entry.pending.map(frame => frame.ready))
        const delay = Math.min(ready - performance.now(), (frame.time - this.audio.currentTime - 0.02) * 1000)
        if (delay > 0) {
          entry.timer = setTimeout(() => this.flushVoice(id, entry), Math.ceil(delay))
          return
        }
        entry.pending.shift()
        if (frame.time > entry.last && frame.time >= this.audio.currentTime)
          this.submit(entry, frame.time, frame.chunk)
      }
    } catch (error) {
      this.fail(error)
    }
  }
  retire(id, entry) {
    clearTimeout(entry.timer)
    entry.pending.length = 0
    if (entry.decoder.state !== 'closed') entry.decoder.close()
    entry.gain?.disconnect()
    this.decoders.delete(id)
  }
  setVolume(user, gain) {
    this.volumes.set(user, gain)
    const entry = this.decoders.get('voice-' + user)
    if (entry?.gain) entry.gain.gain.value = gain
  }
  // Users whose decoded speech is playing now, and the local user while audibly speaking.
  speakers() {
    const users = new Set(this.speaker ? [this.speaker] : []), now = this.audio.currentTime
    for (const entry of this.decoders.values()) if (entry.voiced > now - 0.2) users.add(entry.user)
    return users
  }
  pruneVoice() {
    const now = performance.now()
    for (const [id, entry] of this.decoders) {
      if (id.startsWith('voice-') && now - entry.seen > 30000) this.retire(id, entry)
    }
  }
  reset(streamOnly = false) {
    for (const [id, entry] of this.decoders) {
      if (streamOnly && id.startsWith('voice-')) continue
      this.retire(id, entry)
    }
    for (const [source, stream] of this.sources) {
      if (streamOnly && !stream) continue
      source.stop()
      source.disconnect()
      this.sources.delete(source)
    }
    for (const frame of this.pictures) frame.close()
    this.pictures = []
    this.anchor = null
    if (!streamOnly) this.speaker = null
    this.streamFailed = false
    this.canvas.hidden = true
  }
  fail(error, streamOnly = false) {
    if (this.closed || (streamOnly && this.streamFailed)) return
    if (streamOnly) {
      this.reset(true)
      this.streamFailed = true
    } else this.close()
    this.report(error.message || String(error), streamOnly)
  }
  close() {
    if (this.closed) return
    this.closed = true
    this.abort.abort()
    this.reader?.cancel().catch(() => {})
    cancelAnimationFrame(this.animation)
    this.reset()
    this.audio.close().catch(() => {})
  }
}
