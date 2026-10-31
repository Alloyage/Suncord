// State refreshes, channel navigation, global keys and startup.
function renderSnapshot() {
  $('#username').textContent = snapshot.user?.name || 'Signed out'
  $('#self').querySelector('.avatar')?.replaceWith(userIcon(snapshot.user || { id: '0', name: '?' }))
  renderServers()
  showGuild(selectedGuild === null ? null : guildById(selectedGuild) || guildById(snapshot.activeGuildId) || snapshot.guilds[0])
  if (isForum(channelById(channel))) renderForumPosts()
}
function renderState() {
  renderAccounts()
  renderCall()
  renderTyping()
  notify()
  renderComposerState()
}
function notify() {
  const notices = clientState.notices
  if (features.notices && features.prefs.notifications && 'Notification' in window && Notification.permission === 'granted') {
    for (const item of notices) if (!features.notices.has(item.id) && (document.hidden || !document.hasFocus() || item.channel !== channel)) {
      const notice = new Notification(item.author, { body: item.content, tag: item.channel })
      notice.onclick = () => { focus(); openMessage(item.channel, item.id); notice.close() }
    }
  }
  features.notices = new Set(notices.map(notice => notice.id))
}

// Refreshes run one at a time. Requests made meanwhile coalesce into one more run.
let stateRefresh, refreshQueued = false, forceRefreshQueued = false, appliedSnapshot, stateRetryTimer, stateRetryCount = 0
function refreshState(force = false) {
  refreshQueued = true
  forceRefreshQueued ||= force
  stateRefresh ||= (async () => {
    while (refreshQueued) {
      const forced = forceRefreshQueued
      refreshQueued = forceRefreshQueued = false
      await refreshStateNow(forced)
    }
    clearTimeout(stateRetryTimer)
    stateRetryTimer = null
    stateRetryCount = 0
  })().catch(error => {
    if (stateRetryCount < 3 && !stateRetryTimer) stateRetryTimer = setTimeout(() => {
      stateRetryTimer = null
      refreshState().catch(() => {})
    }, 250 * 2 ** stateRetryCount++)
    throw error
  }).finally(() => { stateRefresh = null })
  return stateRefresh
}
// The applied snapshot's tag. An unchanged snapshot is neither downloaded nor applied again.
let snapshotTag
async function refreshStateNow(force = false) {
  const [result] = await Promise.all([api('state'), preferences])
  const version = value => JSON.stringify([value.account, value.session, value.snapshotRevision])
  const snapshotKey = version(result)
  const changed = force || !snapshot || snapshotKey !== appliedSnapshot
  let saved, tag
  if (changed) {
    const response = await apiResponse('snapshot', { headers: snapshot && snapshotTag ? { 'If-None-Match': snapshotTag } : {} })
    if (response.status !== 304) {
      saved = await response.json()
      tag = response.headers.get('ETag')
      if (version(saved) !== snapshotKey) { refreshQueued = true; return }
    }
  }
  clientState = result
  sampleMedia()
  if (changed) {
    if (saved) {
      const previousAccount = snapshot?.user?.id
      snapshot = saved
      snapshotTag = tag
      snapshot.readable = new Set(snapshot.readable)
      snapshot.sendable = new Set(snapshot.sendable)
      if (snapshot.user) snapshot.users[snapshot.user.id] ||= snapshot.user
      if (previousAccount && previousAccount !== snapshot.user?.id) resetAccount()
      const target = initialChannel && snapshot.user && channelById(initialChannel)
      if (snapshot.user) initialChannel = null
      if (target && target.kind !== 4) {
        selectedGuild = target.guildId || null
        snapshot.activeChannelId = snapshot.visitedChannels[selectedGuild || ''] = target.id
      }
    }
    const selection = request
    renderSnapshot()
    // A selection that renderSnapshot started loads its own features.
    if (request === selection) await refreshFeatures()
    if (channel && clientState.connected && !selectingChannel && !features.loadingHistory && !isForum(channelById(channel))) {
      const current = staleGuard(), previousRows = rows, messages = await api('messages/' + encodeURIComponent(channel))
      if (current() && rows === previousRows && !selectingChannel && !features.loadingHistory && messages !== rows && JSON.stringify(messages) !== JSON.stringify(rows)) {
        rows = messages
        renderTimeline()
      }
    }
  }
  renderState()
  if (changed) appliedSnapshot = snapshotKey
}
async function refreshFeatures() {
  if (!snapshot || !channel) return
  const current = staleGuard()
  const view = await ui('view', { channel, clientId, navigation: request, subscribedIndex: $('#members').hidden ? 0 : features.memberSubscription?.index || 0 })
  if (!current()) return
  features.view = view
  features.context &&= !view.reachLatest
  if (!$('#members').hidden) await refreshMembers()
  const mentions = JSON.stringify(snapshot.mentionCounts)
  if (features.panel?.kind === 'inbox' && features.panel.mentions !== mentions) {
    features.panel.mentions = mentions
    messagePanel('inbox', { reload: true })
  }
  updateMentions(false)
  renderRoleMembers()
  renderUnread()
  renderComposerState()
  markRead()
}

// Resets everything tied to the open channel and switches to next, which may be null.
function leaveChannel(next) {
  if (next !== channel) for (const key of features.collapsed) if (key.startsWith('members:')) features.collapsed.delete(key)
  cancelJump()
  features.picker?.close()
  if (channel) {
    drafts.set(channel, draft.value)
    features.extras.set(channel, { reply: features.reply, sticker: features.sticker })
  }
  const extra = features.extras.get(next) || {}
  Object.assign(features, {
    view: {}, unreadAfter: snapshot.lastRead[next] || '', reply: extra.reply || null, sticker: extra.sticker || null,
    context: false, moreBefore: true, loadingHistory: null, lastRead: '',
    memberIndex: 0, memberTotal: 0, memberRefreshIndex: 0, memberSubscription: null,
  })
  features.memberPages.clear()
  if (!next) {
    if (features.panel) showSidePanel(null)
    features.role?.node.close()
    features.role = null
  }
  channel = next
  request++
  selectingChannel = false
  rows = []
  files = []
  list.replaceChildren()
  draft.value = drafts.get(next) || ''
  $('#history-status').textContent = ''
  $('#member-list').replaceChildren()
  $('#typing').replaceChildren()
  $('#typing').dataset.label = ''
  $('#latest').hidden = true
  renderFiles()
  renderReply()
  renderComposerState()
  updateMentions(false)
  if (clientState) renderCall()
}
async function selectChannel(id) {
  const chosen = channelById(id)
  if (!chosen) { status('Channel is unavailable.'); return }
  snapshot.visitedChannels[chosen.guildId || ''] = id
  if (isForum(chosen)) return openForum(chosen)
  leaveChannel(id)
  selectingChannel = true
  following = false
  const revision = request, current = () => revision === request
  renderChannelSelection(chosen)
  updateMentions()
  recordLocation()
  const featureLoad = refreshFeatures().catch(error => { if (current()) status(error.message) })
  if (features.panel?.kind === 'pins') messagePanel('pins')
  const ready = () => {
    if (!performance.getEntriesByName('chat-ready').length) performance.mark('chat-ready')
    document.documentElement.dataset.ready = 'true'
  }
  try {
    try {
      const cached = await api('messages/' + encodeURIComponent(id))
      if (!current()) return
      rows = cached
      renderTimeline({ follow: false })
      bottom()
      if (rows.length) ready()
    } catch (error) {
      if (!current()) return
      status(error.message)
    }
    const messages = await ui('messages', { channel: id, clientId, navigation: revision })
    if (!current()) return
    rows = messages
    recordLocation()
    await featureLoad
    await refreshFeatures()
    if (!current()) return
    features.context = features.view.reachLatest === false
    renderTimeline({ follow: false })
    await document.fonts.ready
    if (!current()) return
    const boundary = features.unreadAfter && rows.find(message => BigInt(message.id) > BigInt(features.unreadAfter))
    if (boundary) renderTimeline({ follow: false, target: boundary.id })
    const unread = list.querySelector('.unread-boundary')
    if (unread) {
      timeline.scrollTop += unread.getBoundingClientRect().top - timeline.getBoundingClientRect().top - 24
      following = atBottom()
      $('#latest').hidden = following && !features.context
    } else if (features.context) {
      timeline.scrollTop = 0
      $('#latest').hidden = false
    } else bottom()
    resizeDraft()
    draft.focus()
    await new Promise(requestAnimationFrame)
    await new Promise(requestAnimationFrame)
    ready()
  } catch (error) {
    status(error.message)
  } finally {
    if (current()) { selectingChannel = false; markRead() }
  }
}
function openChannel(id) {
  const item = channelById(id)
  if (item) showGuild(guildById(item.guildId) || null, false)
  return selectChannel(id)
}
async function openMessage(channelId, messageId) {
  if (!channelById(channelId)) throw new Error('The original channel is unavailable in this account.')
  if (!snapshot.readable.has(channelId)) throw new Error('You cannot read this channel.')
  if (channel !== channelId) await openChannel(channelId)
  if (channel !== channelId || selectingChannel) return
  if (jumpTimelineMessage(messageId)) $('#latest').hidden = false
  else await loadHistory('before', messageId)
}
function recordLocation() {
  if (features.navigating || !channel || !snapshot.user) return
  const state = { account: snapshot.user.id, channel, guild: selectedGuild }
  if (history.state?.channel !== channel || history.state?.account !== state.account) history.pushState(state, '', location.pathname + '?channel=' + encodeURIComponent(channel))
}
addEventListener('popstate', async event => {
  if (!event.state || event.state.account !== snapshot.user?.id) return
  features.navigating = true
  try {
    showGuild(guildById(event.state.guild) || null, false)
    await selectChannel(event.state.channel)
  } finally { features.navigating = false }
})

function jumpToChannel() {
  const node = modal('Jump to channel'), input = element('input'), options = element('div')
  input.type = 'search'
  input.placeholder = 'Find a channel'
  input.oninput = () => {
    const query = input.value.toLowerCase()
    const found = [...snapshot.channels, ...snapshot.dms].filter(c => c.kind !== 4 && snapshot.readable.has(c.id) && c.name.toLowerCase().includes(query))
    options.replaceChildren(...found.slice(0, 40).map(c => button(c.name, () => { node.close(); openChannel(c.id) })))
  }
  node.append(input, options)
  input.dispatchEvent(new Event('input'))
  input.focus()
}
document.addEventListener('keydown', event => {
  if (event.defaultPrevented || event.isComposing || selectTimeline(event)) return
  if (event.key === 'Escape') {
    if (features.picker?.open) features.picker.close()
    else if (!dialogOpen() && features.reply) { features.reply = null; renderReply() }
  } else if (event.ctrlKey && event.key.toLowerCase() === 'k') {
    event.preventDefault()
    jumpToChannel()
  } else if (!event.ctrlKey && !event.metaKey && !event.altKey && event.key.length === 1 && !dialogOpen() && !event.target.closest('input,textarea,button,select,[contenteditable]') && canSend()) {
    event.preventDefault()
    insertDraft(event.key)
  }
})
document.addEventListener('visibilitychange', () => {
  updateTimelineAnimations()
  markRead()
  if (!document.hidden) wakeLiveUpdates()
})
addEventListener('focus', markRead)
$('#self').onclick = () => snapshot?.user && showProfile(snapshot.user)
$('#channel-name').onclick = () => modal($('#channel-name').textContent).append(element('p', 'channel-info', features.view.details || ''))
$('#reconnect-button').onclick = () => ui('reconnect').catch(error => status(error.message))

// Server events trigger at most four refreshes a second. Polling covers a closed event stream.
let stopLiveUpdates, wakeLiveUpdates = () => {}
function startLiveUpdates() {
  stopLiveUpdates?.()
  const source = new EventSource('/api/events')
  let timer, last = -Infinity
  const update = () => {
    timer ||= setTimeout(() => {
      timer = null
      last = performance.now()
      refreshState().catch(error => status(error.message))
    }, Math.max(0, 250 - (performance.now() - last)))
  }
  source.onopen = source.onmessage = update
  const fallback = setInterval(() => { if (source.readyState !== EventSource.OPEN) update() }, 1000)
  wakeLiveUpdates = () => { last = -Infinity; update() }
  stopLiveUpdates = () => {
    source.close()
    clearInterval(fallback)
    clearTimeout(timer)
    clearTimeout(stateRetryTimer)
    stateRetryTimer = null
    wakeLiveUpdates = () => {}
  }
}
addEventListener('pagehide', () => stopLiveUpdates?.())
addEventListener('pageshow', event => { if (event.persisted) startLiveUpdates() })

async function start() {
  const token = location.hash.slice(1)
  history.replaceState(null, '', location.pathname + location.search)
  if (token) await post('session', { token })
  loadPreferences()
  startLiveUpdates()
  await refreshState(true)
  if (!clientState.connected) accountMenu.showPopover()
}
start().catch(error => status(error.message))
