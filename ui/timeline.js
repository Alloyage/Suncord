// The timeline renders a window of rows around the viewport and patches retained message nodes in place.
const timelineWindow = { source: null, channel: null, heights: new Map(), offsets: [0], order: [], ids: new Map(), end: 0 }
const topSpacer = element('div', 'timeline-spacer'), bottomSpacer = element('div', 'timeline-spacer')
const timelineMedia = new Map(), timelineReveals = new Map()
const messageSizes = new ResizeObserver(() => renderTimeline())
let windowFrame = 0, layoutAnchor, wholeTimelineSelected = false, cancelJump = () => {}, timelineContext = {}

// Lookups that change a message's rendering without changing the message.
function timelineRevision() {
  if (timelineContext.snapshot !== snapshot || timelineContext.roles !== features.view.roles) timelineContext = { snapshot, roles: features.view.roles, revision: (timelineContext.revision || 0) + 1 }
  return timelineContext.revision
}
function timelineNodeKey(node) {
  if (node.nodeType !== Node.ELEMENT_NODE) return node.nodeType
  return [node.tagName, node.className.replace(/\brevealed\b/gu, '').trim(), node._attachmentKey || node.dataset.id || node.getAttribute('src') || node.getAttribute('aria-label') || ''].join(':')
}
// Patching copies attributes and only the on* handler properties below, so timeline nodes
// must use on* handlers rather than addEventListener.
function patchTimelineNode(current, next) {
  if (current._attachmentKey && current._attachmentKey === next._attachmentKey) return
  if (current.nodeType !== Node.ELEMENT_NODE) {
    if (current.nodeValue !== next.nodeValue) current.nodeValue = next.nodeValue
    return
  }
  if (current.matches('.spoiler.revealed') && current.textContent === next.textContent) next.classList.add('revealed')
  for (const attribute of [...current.attributes]) if (!next.hasAttribute(attribute.name)) current.removeAttribute(attribute.name)
  for (const attribute of next.attributes) if (current.getAttribute(attribute.name) !== attribute.value) current.setAttribute(attribute.name, attribute.value)
  for (const handler of ['onclick', 'onkeydown', 'ondblclick', 'oncontextmenu', 'ondragstart', 'ondragover', 'ondrop']) current[handler] = next[handler]
  const available = [...current.childNodes]
  let cursor = current.firstChild
  for (const child of [...next.childNodes]) {
    const key = timelineNodeKey(child), at = available.findIndex(node => timelineNodeKey(node) === key)
    const retained = at < 0 ? child : available.splice(at, 1)[0]
    if (retained !== cursor) current.insertBefore(retained, cursor)
    if (retained !== child) patchTimelineNode(retained, child)
    cursor = retained.nextSibling
  }
  for (const child of available) child.remove()
}
function messageNode(id) { return list.querySelector(`[data-id="${CSS.escape(id)}"]`) }
function renderedMessages() { return [...list.querySelectorAll(':scope > .message')] }
function rebuildTimelineOffsets() {
  const state = timelineWindow
  state.offsets = [0]
  for (const message of rows) state.offsets.push(state.offsets.at(-1) + (state.heights.get(message.id) || 64))
}
function timelinePosition(y) {
  const offsets = timelineWindow.offsets
  let low = 0, high = rows.length
  while (low < high) {
    const mid = (low + high + 1) >> 1
    if (offsets[mid] <= y) low = mid
    else high = mid - 1
  }
  return Math.min(low, Math.max(0, rows.length - 1))
}
function captureTimelineAnchor() {
  const top = timeline.getBoundingClientRect().top
  let node = timelineWindow.order[timelinePosition(timeline.scrollTop)]
  node = node && messageNode(node)
  // Heights can change before the observer runs. Only inspect the bounded window.
  if (!node || node.getBoundingClientRect().bottom <= top || node.getBoundingClientRect().top > top) node = renderedMessages().find(node => node.getBoundingClientRect().bottom > top)
  return node && { id: node.dataset.id, offset: node.getBoundingClientRect().top - top }
}
function rememberTimelineAnchor(force = false) {
  if (!force && layoutAnchor?.node.isConnected && layoutAnchor.node.offsetTop !== layoutAnchor.position) return
  const anchor = captureTimelineAnchor(), node = anchor && messageNode(anchor.id)
  layoutAnchor = node && { node, position: node.offsetTop }
}
function restoreTimelineLayout() {
  if (!layoutAnchor?.node.isConnected) return
  const position = layoutAnchor.node.offsetTop, delta = position - layoutAnchor.position
  if (delta) { timeline.scrollTop += delta; layoutAnchor.position = position }
}
// Scrolling and selection must preserve the current position, even near bottom.
function queueTimelineWindow() {
  if (!windowFrame) windowFrame = requestAnimationFrame(() => { windowFrame = 0; renderTimeline({ follow: false }) })
}
function renderTimeline({ follow = following, target = null } = {}) {
  if (!follow) restoreTimelineLayout()
  const state = timelineWindow, anchor = captureTimelineAnchor(), revision = timelineRevision()
  let scrollY = timeline.scrollTop
  if (state.channel !== channel) {
    messageSizes.disconnect()
    mediaRetries.clear()
    timelineMedia.clear()
    timelineReveals.clear()
    state.heights.clear()
    state.channel = channel
    state.source = null
  }
  if (state.source !== rows) {
    // Newer pages and live arrivals can evict older rows from the server's window.
    if (state.order.length && rows.length && BigInt(rows[0].id) > BigInt(state.order[0])) features.moreBefore = true
    if (!list.querySelector('.message')) messageSizes.disconnect()
    state.source = rows
    state.order = rows.map(message => message.id)
    state.ids = new Map(rows.map((message, index) => [message.id, index]))
    for (const id of state.heights.keys()) if (!state.ids.has(id)) state.heights.delete(id)
    rebuildTimelineOffsets()
    if (!follow && anchor && state.ids.has(anchor.id)) scrollY = state.offsets[state.ids.get(anchor.id)] - anchor.offset
  }
  if (target && state.ids.has(target)) scrollY = state.offsets[state.ids.get(target)] - timeline.clientHeight / 2
  const y = follow ? Math.max(0, state.offsets.at(-1) - timeline.clientHeight) : scrollY
  let start = timelinePosition(Math.max(0, y - 800)), end = Math.min(rows.length, timelinePosition(y + timeline.clientHeight + 800) + 1)
  const selection = getSelection()
  if (selection && !selection.isCollapsed && (list.contains(selection.anchorNode) || list.contains(selection.focusNode)) && !target && !wholeTimelineSelected) {
    // Keep the native range intact while extending the window toward the pointer.
    const range = selection.getRangeAt(0)
    const article = node => (node.nodeType === Node.ELEMENT_NODE ? node : node.parentElement)?.closest('.message')
    let first = state.ids.get(article(range.startContainer)?.dataset.id), last = state.ids.get(article(range.endContainer)?.dataset.id)
    if (first === undefined || last === undefined) {
      const selected = renderedMessages().filter(node => state.ids.has(node.dataset.id) && range.intersectsNode(node))
      first = state.ids.get(selected[0]?.dataset.id)
      last = state.ids.get(selected.at(-1)?.dataset.id)
    }
    if (first !== undefined && last !== undefined) { start = Math.min(start, first); end = Math.max(end, last + 1) }
  }
  state.end = end
  const current = new Map(renderedMessages().map(node => [node.dataset.id, node]))
  for (const [id, node] of current) {
    const index = state.ids.get(id)
    if (index !== undefined && index >= start && index < end) continue
    rememberTimelineState(node)
    messageSizes.unobserve(node)
    node.remove()
    current.delete(id)
  }
  if (topSpacer.parentNode !== list) list.prepend(topSpacer)
  if (bottomSpacer.parentNode !== list) list.append(bottomSpacer)
  let cursor = topSpacer.nextSibling
  for (let index = start; index < end; index++) {
    const message = rows[index], previous = rows[index - 1]
    const signature = JSON.stringify([message, previous && [previous.id, previous.authorId, previous.timestamp], features.unreadAfter]) + revision
    let node = current.get(message.id)
    if (!node) { node = renderMessage(message, index); messageSizes.observe(node) }
    else if (node._renderSignature !== signature) patchTimelineNode(node, renderMessage(message, index))
    if (node._renderSignature !== signature) restoreTimelineState(node)
    node._renderSignature = signature
    if (node !== cursor) list.insertBefore(node, cursor)
    cursor = node.nextSibling
  }
  let changed = false
  for (const node of renderedMessages()) {
    const height = node.getBoundingClientRect().height
    if (height && state.heights.get(node.dataset.id) !== height) { state.heights.set(node.dataset.id, height); changed = true }
  }
  if (changed) rebuildTimelineOffsets()
  topSpacer.style.height = state.offsets[start] + 'px'
  bottomSpacer.style.height = state.offsets.at(-1) - state.offsets[end] + 'px'
  timeline.scrollTop = scrollY
  if (follow) bottom(false)
  else if (anchor && !target) {
    const node = messageNode(anchor.id)
    if (node) timeline.scrollTop += node.getBoundingClientRect().top - timeline.getBoundingClientRect().top - anchor.offset
  }
  rememberTimelineAnchor(true)
  updateTimelineAnimations()
}
function updateTimelineAnimations() {
  const viewport = timeline.getBoundingClientRect()
  for (const media of list.querySelectorAll('video[data-animated]')) {
    const bounds = media.getBoundingClientRect()
    if (!document.hidden && bounds.bottom > viewport.top && bounds.top < viewport.bottom) { if (media.paused) media.play().catch(() => {}) }
    else if (!media.paused) media.pause()
  }
}

// Revealed spoilers and media playback survive nodes leaving the window.
function timelineMediaKey(node, media, index) {
  const url = new URL(media.currentSrc || media.src, location.href)
  return `${node.dataset.id}:${index}:${url.origin}${url.pathname}`
}
function remember(cache, key, value) {
  cache.delete(key)
  cache.set(key, value)
  if (cache.size > 128) cache.delete(cache.keys().next().value)
}
function rememberTimelineState(node) {
  const spoilers = [...node.querySelectorAll('.spoiler')]
  const revealed = spoilers.flatMap((part, index) => part.classList.contains('revealed') ? [index] : [])
  timelineReveals.delete(node.dataset.id)
  if (revealed.length) remember(timelineReveals, node.dataset.id, { content: JSON.stringify(spoilers.map(part => part.textContent)), revealed })
  node.querySelectorAll('video:not([data-animated]),audio').forEach((media, index) => {
    const key = timelineMediaKey(node, media, index)
    if (media.currentTime || media.volume !== 1 || media.muted || media.playbackRate !== 1 || timelineMedia.has(key)) remember(timelineMedia, key, { time: media.currentTime, volume: media.volume, muted: media.muted, rate: media.playbackRate })
    media.pause()
  })
}
function restoreTimelineState(node) {
  const spoilers = [...node.querySelectorAll('.spoiler')], saved = timelineReveals.get(node.dataset.id)
  if (!node._renderSignature && saved?.content === JSON.stringify(spoilers.map(part => part.textContent))) for (const index of saved.revealed) spoilers[index]?.classList.add('revealed')
  node.querySelectorAll('video:not([data-animated]),audio').forEach((media, index) => {
    if (media._timelineRestored) return
    media._timelineRestored = true
    const saved = timelineMedia.get(timelineMediaKey(node, media, index))
    if (!saved) return
    media.volume = saved.volume
    media.muted = saved.muted
    media.playbackRate = saved.rate
    const seek = () => { media.currentTime = Number.isFinite(media.duration) ? Math.min(saved.time, media.duration) : saved.time }
    if (media.readyState) seek()
    else media.addEventListener('loadedmetadata', seek, { once: true })
  })
}

// Ctrl+A selects the whole loaded history, which copies as plain text.
function selectTimeline(event) {
  if (!(event.ctrlKey || event.metaKey) || event.key.toLowerCase() !== 'a' || event.target.closest('input,textarea,[contenteditable],dialog')) return false
  if (!list.contains(getSelection()?.anchorNode) && !event.target.closest('#timeline')) return false
  event.preventDefault()
  const range = document.createRange(), selection = getSelection()
  range.selectNodeContents(list)
  selection.removeAllRanges()
  selection.addRange(range)
  wholeTimelineSelected = true
  return true
}
list.addEventListener('pointerdown', () => { wholeTimelineSelected = false })
document.addEventListener('selectionchange', () => {
  const selection = getSelection(), range = selection?.rangeCount && selection.getRangeAt(0)
  if (wholeTimelineSelected && (!range || range.startContainer !== list || range.startOffset !== 0 || range.endContainer !== list || range.endOffset !== list.childNodes.length)) wholeTimelineSelected = false
  queueTimelineWindow()
})
document.addEventListener('copy', event => {
  if (!wholeTimelineSelected || getSelection()?.isCollapsed || !list.contains(getSelection()?.anchorNode)) return
  event.preventDefault()
  event.clipboardData.setData('text/plain', rows.map(message => `${message.author} — ${messageTimestamp(new Date(message.timestamp))}\n${message.content}${message.attachments ? '\n' + message.attachments.map(file => file.url).join('\n') : ''}`).join('\n\n'))
})

function atBottom() { return timeline.scrollHeight - timeline.clientHeight - timeline.scrollTop <= 1 }
function jumpTimelineMessage(id) {
  if (!rows.some(message => message.id === id)) return false
  following = false
  renderTimeline({ follow: false, target: id })
  const node = messageNode(id)
  node?.scrollIntoView({ block: 'center' })
  node?.classList.add('highlighted')
  return true
}
function bottom(render = true) {
  if (render && timelineWindow.source === rows && timelineWindow.end < rows.length) renderTimeline({ follow: true })
  timeline.scrollTop = timeline.scrollHeight
  following = true
  $('#latest').hidden = true
}
function smoothBottom() {
  cancelJump()
  const start = timeline.scrollTop, end = timeline.scrollHeight - timeline.clientHeight
  if (matchMedia('(prefers-reduced-motion: reduce)').matches || end <= start) { bottom(); return }
  const started = performance.now()
  let frame
  const cancel = () => {
    cancelAnimationFrame(frame)
    timeline.removeEventListener('wheel', cancel)
    timeline.removeEventListener('pointerdown', cancel)
  }
  cancelJump = cancel
  const step = now => {
    const fraction = Math.min(1, (now - started) / 260)
    timeline.scrollTop = start + (end - start) * (1 - (1 - fraction) ** 3)
    if (fraction < 1) frame = requestAnimationFrame(step)
    else { cancel(); bottom(); markRead() }
  }
  timeline.addEventListener('wheel', cancel, { passive: true })
  timeline.addEventListener('pointerdown', cancel)
  frame = requestAnimationFrame(step)
}
async function loadHistory(direction = 'before', around = null) {
  const loading = features.loadingHistory
  if (loading?.channel === channel && loading.revision === request) {
    if (direction !== 'latest' && !around) return
    const current = staleGuard()
    await loading.done
    return current() ? loadHistory(direction, around) : undefined
  }
  if (selectingChannel || !channel || (!rows.length && !around && direction !== 'latest')) return
  if (!around && (direction === 'before' && !features.moreBefore || direction === 'after' && !features.context)) return
  const current = staleGuard(), pending = { channel, revision: request }
  pending.done = new Promise(resolve => { pending.finish = resolve })
  features.loadingHistory = pending
  $('#history-status').textContent = 'Loading messages…'
  try {
    const values = { channel, clientId, navigation: request }
    if (around) values.around = around
    else if (direction === 'before' && rows.length) values.before = rows[0].id
    else if (direction === 'after' && rows.length) values.after = rows.at(-1).id
    const result = await ui('history', values)
    if (!current()) return
    rows = result.rows
    features.context = !result.reachLatest
    if (direction === 'before' || direction === 'latest' || around) features.moreBefore = result.more
    if (around) following = false
    renderTimeline({ follow: direction === 'latest' })
    if (around) jumpTimelineMessage(around)
    $('#latest').hidden = !features.context && following
    return true
  } catch (error) {
    if (current()) status(error.message)
    return false
  } finally {
    if (features.loadingHistory === pending) { features.loadingHistory = null; $('#history-status').textContent = '' }
    pending.finish()
  }
}
async function latestMessages() {
  if (!channel || features.context && !await loadHistory('latest')) return
  smoothBottom()
}
async function markRead() {
  if (selectingChannel || !channel || !following || features.context || !document.hasFocus() || document.hidden || dialogOpen() || !rows.length) return
  const id = channel, message = rows.at(-1).id, account = snapshot.user?.id, key = id + ':' + message
  if (features.lastRead === key) return
  features.lastRead = key
  try {
    await ui('read', { channel: id }, account)
  } catch (error) {
    if (features.lastRead === key && snapshot.user?.id === account) {
      features.lastRead = ''
      status(error.message)
    }
  }
}

new ResizeObserver(() => { if (snapshot) renderTimeline() }).observe(timeline)
timeline.addEventListener('scroll', () => {
  if (!selectingChannel) {
    following = atBottom()
    $('#latest').hidden = following && !features.context
    queueTimelineWindow()
  }
  rememberTimelineAnchor()
  if (timeline.scrollTop < 120) loadHistory('before')
  if (features.context && timeline.scrollTop + timeline.clientHeight >= timeline.scrollHeight - 100) loadHistory('after')
  markRead()
})
$('#latest').onclick = latestMessages
