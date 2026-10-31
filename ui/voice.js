// Voice calls and screen streams. BrowserPlayback in playback.js plays the received media.
let mediaSources, sourcesLoading, joining = false, mediaSample, mediaRates = { encoded: 0, sent: 0 }, speakerTimer
// Others in your call, a call to ring once joined and incoming calls you ignored.
let peers = null, pendingRing = null, voiceSync = null, syncedVoice = null, wasActive = false
const ignoredRings = new Set()
const stored = (key, fallback) => { try { return JSON.parse(localStorage[key]) ?? fallback } catch { return fallback } }
// Self mute and deafen carry over between calls like the official client.
let selfVoice = stored('selfVoice', { mute: false, deaf: false })
let streamMuted = stored('streamMuted', false)
const userVolumes = stored('userVolumes', {})

function callActive() {
  const phase = clientState?.media?.phase
  return !!phase && phase !== 'Disconnected' && phase !== 'Failed'
}
function mediaScope() { return { session: clientState.session, call: clientState.media.id } }
function sameMedia(scope) { return scope.session === clientState?.session && scope.call === clientState?.media?.id }
function callOngoing(id) { return !!snapshot.calls?.[id] || !!snapshot.voice?.[id]?.length }
// Frame rates come from counter deltas between state refreshes.
function sampleMedia() {
  const media = clientState.media, sample = { time: performance.now(), encoded: media?.video_encoded || 0, sent: media?.video_frames || 0 }
  const elapsed = mediaSample && (sample.time - mediaSample.time) / 1000
  if (!mediaSample || sample.encoded < mediaSample.encoded || sample.sent < mediaSample.sent) {
    mediaRates = { encoded: 0, sent: 0 }
    mediaSample = sample
  } else if (elapsed >= 0.5) {
    mediaRates = { encoded: (sample.encoded - mediaSample.encoded) / elapsed, sent: (sample.sent - mediaSample.sent) / elapsed }
    mediaSample = sample
  }
}
// People in a voice channel with their server names, sorted by name.
// Cached per snapshot, as the sidebar and call stage ask for the same lists on every render.
function voiceUsers(id, guild) {
  const cache = indexed().voice, key = id + ':' + guild
  if (!cache.has(key)) cache.set(key, (snapshot.voice?.[id] || [])
    .map(state => ({ ...state, user: contextualUser(state.id, guild) || { id: state.id, name: 'Unknown' } }))
    .sort((a, b) => a.user.name.localeCompare(b.user.name)))
  return cache.get(key)
}
function voiceTags({ mute, deaf, serverMute, serverDeaf, stream, video }) {
  const quiet = serverDeaf ? 'Server deafened' : deaf ? 'Deafened' : serverMute ? 'Server muted' : mute ? 'Muted' : ''
  return [[quiet, quiet, serverDeaf || (serverMute && !deaf) ? 'server' : ''], [video, 'Video'], [stream, 'Live', 'live']]
    .filter(([shown]) => shown).map(([, label, kind]) => element('span', kind ? 'voice-tag ' + kind : 'voice-tag', label))
}
// Marks a voice participant for speaking rings and opens their volume on right click.
function voicePerson(node, state) {
  node.dataset.speaker = state.id
  if (state.id !== snapshot.user?.id) node.oncontextmenu = event => { event.preventDefault(); volumeDialog(state.user) }
}
function showSpeakers() {
  const users = playback?.speakers() || new Set()
  for (const node of document.querySelectorAll('[data-speaker]')) node.classList.toggle('speaking', users.has(node.dataset.speaker))
}
function userVolume(id) {
  const { volume = 100, muted = false } = userVolumes[id] || {}
  return muted ? 0 : volume / 100
}
function applyAudio() {
  if (!playback) return
  playback.output.gain.value = selfVoice.deaf ? 0 : 1
  playback.streamOutput.gain.value = streamMuted ? 0 : 1
  for (const id in userVolumes) playback.setVolume(id, userVolume(id))
}
function volumeDialog(user) {
  const node = modal(user.name, 'volume-dialog'), setting = { volume: 100, muted: false, ...userVolumes[user.id] }
  const volume = element('label', '', 'User volume'), range = element('input'), value = element('output', '', setting.volume + '%')
  const mute = element('label', '', 'Mute'), muted = element('input')
  range.type = 'range'
  range.min = 0
  range.max = 200
  range.value = setting.volume
  muted.type = 'checkbox'
  muted.checked = setting.muted
  volume.append(range, value)
  mute.prepend(muted)
  range.oninput = muted.onchange = () => {
    setting.volume = Number(range.value)
    setting.muted = muted.checked
    value.textContent = setting.volume + '%'
    if (setting.volume === 100 && !setting.muted) delete userVolumes[user.id]
    else userVolumes[user.id] = { ...setting }
    localStorage.userVolumes = JSON.stringify(userVolumes)
    playback?.setVolume(user.id, userVolume(user.id))
  }
  node.append(volume, mute)
}
// Discord's cues use separate audio so leaving a call cannot cut off its disconnect sound.
const voiceSounds = new Map()
let ringingCall = null
function voiceSound(name, local = false) {
  if (selfVoice.deaf && !local) return
  if (!voiceSounds.has(name)) voiceSounds.set(name, new Audio('/sounds/' + name + '.mp3'))
  const audio = voiceSounds.get(name)
  audio.currentTime = 0
  audio.play().catch(() => {})
}
// Only confirmed calls and streams make sounds, rather than pending or failed attempts.
function chime(active, media) {
  const connected = active && playback && media.phase !== 'Joining'
  const id = media?.channel_id
  const others = connected ? (snapshot.voice?.[id] || []).map(state => state.id).filter(user => user !== snapshot.user?.id) : null
  const sharing = !!connected && (!!media.sharing || media.stream_phase === 'Waiting for a viewer')
  const viewers = sharing ? media.viewers || [] : []
  if (others && peers?.id === id) {
    if (sharing !== peers.sharing) voiceSound(sharing ? 'stream_started' : 'stream_ended', true)
    else if (sharing && viewers.some(user => !peers.viewers.has(user))) voiceSound('stream_user_joined')
    else if (sharing && [...peers.viewers].some(user => !viewers.includes(user))) voiceSound('stream_user_left')
    else if (others.some(user => !peers.users.has(user))) voiceSound('user_join')
    else if ([...peers.users].some(user => !others.includes(user))) voiceSound('user_leave')
  } else if (others || peers) voiceSound(others ? 'user_join' : 'disconnect', true)
  peers = others && { id, users: new Set(others), sharing, viewers: new Set(viewers) }
}
// An incoming call shows until you join or ignore it, or it stops ringing you.
function renderRing(media) {
  const me = snapshot.user?.id, rings = id => snapshot.calls?.[id]?.includes(me)
  for (const id of ignoredRings) if (!rings(id)) ignoredRings.delete(id)
  const ring = Object.keys(snapshot.calls || {}).find(id => rings(id) && id !== media?.channel_id && !ignoredRings.has(id) && channelById(id))
  const audibleRing = selfVoice.deaf ? null : ring
  if (ringingCall !== audibleRing) {
    voiceSounds.get('call_ringing')?.pause()
    ringingCall = audibleRing
    if (audibleRing) {
      voiceSound('call_ringing')
      voiceSounds.get('call_ringing').loop = true
    }
  }
  $('#incoming-call').hidden = !ring
  if (!ring) return
  $('#incoming-call').dataset.channel = ring
  $('#incoming-name').textContent = channelById(ring).name + ' is calling'
}
function renderCall() {
  const media = clientState.media, active = callActive(), streaming = !!media?.stream_phase || media?.sharing
  const quality = media?.video_width && streaming
    ? `${media.video_width}×${media.video_height} · ${mediaRates.sent.toFixed(0)} fps sent (${media.video_fps} target) · ${(media.video_bitrate / 1e6).toFixed(1)} Mbps · ${media.video_encoder.startsWith('nv') ? 'NVIDIA' : 'Software'}` : ''
  // A private call you are not in shows its people and a join button.
  const viewed = channelById(channel), callable = viewed?.kind === 1 || viewed?.kind === 3
  const preview = !active && callable && callOngoing(channel)
  const room = preview ? viewed : channelById(media?.channel_id), here = !!room && room.id === channel
  chime(active, media)
  // Ring others only after Discord confirms the join, as the call exists by then.
  if (pendingRing && active && media.channel_id === pendingRing && media.phase !== 'Joining') {
    ui('ring', { channel: pendingRing }).catch(error => status(error.message))
    pendingRing = null
  }
  $('#start-call').hidden = !callable || active && here
  $('#start-call').disabled = $('#join-call').disabled = joining || !clientState.ready
  $('#call').hidden = !active && !preview && !media?.error
  $('#call').classList.toggle('compact', !here)
  // A call that failed or moved to another client can be joined again from its stage.
  const rejoin = !active && !preview && !!media?.error && media?.channel_id
  $('#join-call').hidden = !preview && !rejoin
  $('#join-call').textContent = rejoin ? 'Rejoin' : 'Join call'
  $('#join-call').dataset.channel = rejoin || channel
  $('#call .call-controls').hidden = !active
  const roomName = room?.name || 'Voice call'
  if ($('#call-return').textContent !== roomName) $('#call-return').replaceChildren(icon('voice'), element('span', '', roomName))
  const ping = media?.ping && media.phase === 'Encrypted voice' ? media.ping + ' ms' : ''
  // Encryption starts once someone else joins, so a lone caller is waiting rather than negotiating.
  const alone = media?.phase === 'Negotiating encryption' && voiceUsers(media.channel_id, room?.guildId).every(state => state.id === snapshot.user?.id)
  $('#call-status').textContent = preview ? 'Call in progress' : media?.error || media?.stream_error || media?.watch_error || [alone ? 'Waiting for others' : media?.phase, ping, media?.stream_phase, media?.watch_phase, quality].filter(Boolean).join(' · ')
  $('#call-status').title = `Encoded: ${mediaRates.encoded.toFixed(1)} fps · Sent: ${mediaRates.sent.toFixed(1)} fps`
  $('#share-screen').hidden = $('#stream-source').hidden = streaming
  $('#stop-stream').hidden = !streaming
  iconLabel($('#stop-stream'), 'close', 'Stop sharing')
  const stage = $('#stream-stage')
  stage.hidden = !here || !active || !media?.watch_owner
  const previewing = media?.watch_owner === snapshot.user?.id
  $('#stream-mute').hidden = previewing
  iconLabel($('#stream-mute'), streamMuted ? 'headphones-off' : 'headphones', streamMuted ? 'Unmute stream' : 'Mute stream')
  $('#stream-mute').setAttribute('aria-pressed', String(streamMuted))
  iconLabel($('#stop-watching'), 'close', previewing ? 'Stop preview' : 'Stop watching')
  if (stage.hidden && document.fullscreenElement === stage) document.exitFullscreen().catch(() => {})
  const viewers = active && media?.sharing ? media.viewers : [], audience = $('#stream-viewers')
  const users = viewers.map(id => contextualUser(id, room?.guildId) || { id, name: 'Unknown' }), seen = JSON.stringify(users.map(user => [user.id, user.name, user.avatar]))
  if (audience.dataset.signature !== seen) {
    audience.dataset.signature = seen
    audience.replaceChildren(...users.slice(0, 5).map(userIcon), element('span', '', viewers.length + ' watching'))
    audience.title = users.map(user => user.name).join(', ')
  }
  audience.hidden = !viewers.length
  renderRing(media)
  const muted = selfVoice.mute || selfVoice.deaf
  iconLabel($('#mute'), muted ? 'mic-off' : 'mic', muted ? 'Unmute' : 'Mute')
  $('#mute').classList.toggle('off', muted)
  iconLabel($('#deafen'), selfVoice.deaf ? 'headphones-off' : 'headphones', selfVoice.deaf ? 'Undeafen' : 'Deafen')
  $('#deafen').classList.toggle('off', selfVoice.deaf)
  for (const node of $('#channels').querySelectorAll('[data-voice]')) {
    node.classList.toggle('active', active && node.dataset.voice === media?.channel_id)
    node.disabled = joining || !clientState.ready
  }
  const people = active || preview ? voiceUsers(room?.id, room?.guildId) : [], tiles = $('#call-people')
  const scope = mediaScope(), account = snapshot.user?.id
  const signature = JSON.stringify([people, media?.watch_owner, streaming, active, scope, account])
  if (tiles.dataset.signature !== signature) {
    tiles.dataset.signature = signature
    tiles.replaceChildren(...people.map(state => {
      const tile = element('div', 'call-person'), name = element('span', 'voice-name', state.user.name)
      const face = button('', () => showProfile(state.user), 'call-face')
      face.append(userIcon(state.user))
      face.setAttribute('aria-label', 'View ' + state.user.name + ' profile')
      tile.append(face, name, element('span', 'call-tags'))
      tile.lastChild.append(...voiceTags(state))
      voicePerson(tile, state)
      if (active && (state.stream || state.id === snapshot.user?.id && streaming) && state.id !== media?.watch_owner) tile.append(button(state.id === snapshot.user?.id ? 'Preview' : 'Watch', () => mediaAction('watch', { owner: state.id }, scope, account), 'call-watch'))
      return tile
    }))
  }
  if (playback) $('#playback-status').textContent = `${playback.videoDecoded} decoded · ${playback.videoFrames} displayed · ${playback.audioFrames} audio buffers`
  // Mute or deafen pressed during the join reaches the call once it is up.
  if (!joining) syncVoice()
  // A stopped call reports an empty phase, as does a join Discord has not answered yet.
  if (!active && playback && !joining && (media?.phase || wasActive)) closePlayback()
  wasActive = active
  if (clientState.ready && !mediaSources && !sourcesLoading) loadMediaSources().catch(error => status(error.message))
}
function closePlayback() {
  if (peers) voiceSound('disconnect', true)
  peers = pendingRing = ringingCall = null
  voiceSounds.get('call_ringing')?.pause()
  playback?.close()
  playback = null
  clearInterval(speakerTimer)
  speakerTimer = null
  showSpeakers()
}
// Refills a device picker, keeping its choice. An open picker waits until it closes.
function fillSources(control) {
  if (!mediaSources || document.activeElement === control) return
  const monitor = control.id === 'stream-source', selected = control.value, kept = monitor ? 1 : 0
  while (control.options.length > kept) control.remove(kept)
  for (const device of mediaSources.audio.filter(device => device.monitor === monitor)) {
    const option = element('option', '', device.name)
    option.value = device.id
    control.append(option)
  }
  control.value = selected
  const inputs = mediaSources.audio.filter(device => !device.monitor)
  if (!control.value && !monitor) control.value = (inputs.find(device => device.isDefault) || inputs[0])?.id || ''
  // A replaced microphone moves the live call to the shown device.
  if (control.value !== selected && !monitor) control.dispatchEvent(new Event('change'))
}
function loadMediaSources() {
  sourcesLoading ||= action('sources').then(sources => {
    mediaSources = sources
    fillSources($('#microphone'))
    fillSources($('#stream-source'))
  }).finally(() => { sourcesLoading = null })
  return sourcesLoading
}
// Ring rings the other recipients when you start a private call.
async function joinVoice(id, ring = false) {
  if (joining) return
  if (callActive() && clientState.media.channel_id === id && playback && !playback.closed) {
    if (channel !== id) openChannel(id)
    return
  }
  const session = featureSession(), account = snapshot.user?.id
  const previous = playback
  let player, joined = false
  const current = () => session === featureSession() && playback === player && !player.closed
  joining = true
  if (channel !== id && snapshot.readable.has(id)) openChannel(id)
  try {
    if (previous) closePlayback()
    player = playback = new BrowserPlayback($('#stream-video'), (error, streamOnly) => {
      if (playback !== player || session !== featureSession()) return
      status(error)
      action(streamOnly ? 'stopWatching' : 'stop', {}, account, player.scope).catch(() => {})
    }, account, clientState.session)
    applyAudio()
    speakerTimer ||= setInterval(showSpeakers, 100)
    // Resume during the click, before waiting for the previous call to leave.
    await player.audio.resume()
    if (!current()) return
    if (previous) await action('stop', {}, account, previous.scope)
    if (!current()) return
    await player.start()
    if (!current()) return
    await loadMediaSources()
    if (!current()) return
    pendingRing = ring ? id : null
    const sent = selfVoice
    await action('join', { channel: id, source: $('#microphone').value || null, ...sent }, account, player.scope)
    joined = true
    await refreshState()
    if (current()) syncedVoice = sent
  } catch (error) {
    if (session === featureSession()) status(error.message)
    if (playback === player) {
      pendingRing = null
      if (!joined || player?.closed) closePlayback()
    }
  } finally {
    if (playback === player && session !== featureSession()) closePlayback()
    joining = false
    renderState()
  }
}
async function mediaAction(name, values = {}, scope = mediaScope(), account = snapshot.user?.id) {
  try {
    await action(name, values, account, scope)
    await refreshState()
  } catch (error) { status(error.message) }
}
function setVoice(next) {
  if (next.deaf !== selfVoice.deaf) voiceSound(next.deaf ? 'deafen' : 'undeafen', true)
  else if (next.mute !== selfVoice.mute) voiceSound(next.mute ? 'mute' : 'unmute', true)
  selfVoice = next
  localStorage.selfVoice = JSON.stringify(next)
  applyAudio()
  renderCall()
  syncVoice()
}
// One request at a time, always sending the latest choice, so a slow toggle cannot land last.
function syncVoice() {
  if (voiceSync || !callActive() || syncedVoice === selfVoice) return
  // A failed request stays unsynced, so a later render retries it.
  const next = selfVoice, scope = mediaScope(), account = snapshot.user?.id
  voiceSync = action('mute', next, account, scope).then(() => { if (sameMedia(scope)) syncedVoice = next }, error => { if (sameMedia(scope)) status(error.message) })
    .finally(() => { voiceSync = null })
    .then(() => syncedVoice === next && syncVoice())
}

$('#leave').onclick = () => { closePlayback(); mediaAction('stop') }
$('#call-return').onclick = () => { const id = clientState?.media?.channel_id; if (id && id !== channel) openChannel(id) }
$('#microphone').onchange = () => { if (callActive()) mediaAction('microphone', { source: $('#microphone').value || null }) }
for (const control of [$('#microphone'), $('#stream-source')]) {
  control.onpointerenter = control.onfocus = () => loadMediaSources().catch(error => status(error.message))
  control.onblur = () => fillSources(control)
}
// Unmuting while deafened also undeafens. Undeafening restores the earlier mute.
$('#mute').onclick = () => setVoice(selfVoice.deaf ? { mute: false, deaf: false } : { ...selfVoice, mute: !selfVoice.mute })
$('#deafen').onclick = () => setVoice({ ...selfVoice, deaf: !selfVoice.deaf })
$('#share-screen').onclick = async () => {
  try {
    await loadMediaSources()
    const audio_source = $('#stream-source').value || null
    if (!mediaSources.screens.length) return mediaAction('share', { audio_source })
    const scope = mediaScope(), account = snapshot.user?.id
    const picker = modal('Share a screen or window')
    for (const source of mediaSources.screens) picker.append(button(source.name, () => {
      picker.close()
      mediaAction('share', { audio_source, video_source: source.id }, scope, account)
    }, 'capture-source'))
  } catch (error) { status(error.message) }
}
$('#stop-stream').onclick = () => mediaAction('stopStream')
$('#stop-watching').onclick = () => mediaAction('stopWatching')
$('#stream-mute').onclick = () => {
  streamMuted = !streamMuted
  localStorage.streamMuted = JSON.stringify(streamMuted)
  applyAudio()
  renderCall()
}
$('#start-call').onclick = () => joinVoice(channel, !callOngoing(channel))
$('#join-call').onclick = () => joinVoice($('#join-call').dataset.channel)
$('#incoming-join').onclick = () => joinVoice($('#incoming-call').dataset.channel)
$('#incoming-ignore').onclick = () => { ignoredRings.add($('#incoming-call').dataset.channel); renderCall() }
$('#stream-fullscreen').onclick = $('#stream-video').ondblclick = () =>
  (document.fullscreenElement ? document.exitFullscreen() : $('#stream-stage').requestFullscreen()).catch(error => status(error.message))
// Ctrl+Shift+M and Ctrl+Shift+D work like the mute and deafen buttons.
document.addEventListener('keydown', event => {
  const key = event.key.toLowerCase()
  if (!event.ctrlKey || !event.shiftKey || event.altKey || event.metaKey || (key !== 'm' && key !== 'd')) return
  event.preventDefault()
  $(key === 'm' ? '#mute' : '#deafen').click()
})
addEventListener('pagehide', closePlayback)
