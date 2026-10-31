// Shared state, DOM helpers, requests and lookups. The other scripts load after this one and share its globals.
const clientId = crypto.randomUUID()
const $ = selector => document.querySelector(selector)
const app = $('#app'), timeline = $('#timeline'), list = $('#messages')
let snapshot, clientState, channel, selectedGuild, structure, playback
let rows = [], files = [], following = true, request = 0, selectingChannel = false
let initialChannel = new URLSearchParams(location.search).get('channel')
const drafts = new Map()
const features = {
  prefs: { avatarRoundness: 100, showInaccessibleChannels: false, notifications: true, channelPanelWidth: 252, sidePanelWidth: 260 },
  view: {}, unreadAfter: '', reply: null, sticker: null, extras: new Map(), collapsed: new Set(),
  memberIndex: 0, memberPages: new Map(), memberTotal: 0, memberRefreshIndex: 0, membersBusy: false,
  panel: null, notices: null, context: false, moreBefore: true, loadingHistory: null, navigating: false,
  slowUntil: new Map(), emoji: null, picker: null, role: null, lastRead: '',
}

function element(tag, className, text) {
  const node = document.createElement(tag)
  if (className) node.className = className
  if (text !== undefined) node.textContent = text
  return node
}
function icon(name) {
  const node = document.createElementNS('http://www.w3.org/2000/svg', 'svg')
  node.setAttribute('class', 'icon')
  const use = document.createElementNS(node.namespaceURI, 'use')
  use.setAttribute('href', '/icons.svg#' + name)
  node.append(use)
  node.setAttribute('aria-hidden', 'true')
  return node
}
function iconLabel(node, name, label) {
  if (node.dataset.icon === name && node.title === label) return node
  node.dataset.icon = name
  node.replaceChildren(icon(name))
  node.title = label
  node.setAttribute('aria-label', label)
  return node
}
// Buttons disable themselves while their action runs and report failures in the status line.
function button(label, run, className = '') {
  const node = element('button', className, label)
  node.type = 'button'
  node.onclick = async event => {
    const control = event.currentTarget
    event.stopPropagation()
    control.disabled = true
    try { await run(event) } catch (error) { status(error.message) } finally { control.disabled = false }
  }
  return node
}
function modal(title, className = '', anchored = false) {
  const node = element('dialog', className), close = iconLabel(element('button', 'close'), 'close', 'Close ' + title)
  close.type = 'button'
  close.onclick = () => node.close()
  node.append(close, element('h2', '', title))
  const listeners = new AbortController(), signal = listeners.signal
  node.addEventListener('close', () => {
    listeners.abort()
    node.remove()
    if (!document.querySelector('dialog[open],#account-menu:popover-open')) draft.focus()
  }, { once: true })
  document.body.append(node)
  if (anchored) {
    const position = () => {
      const bounds = $('main').getBoundingClientRect(), top = $('#compose').getBoundingClientRect().top
      node.style.width = Math.min(464, bounds.width - 32) + 'px'
      node.style.right = innerWidth - bounds.right + 16 + 'px'
      node.style.bottom = innerHeight - top + 8 + 'px'
      node.style.height = Math.min(448, top - 72) + 'px'
    }
    position()
    node.show()
    addEventListener('resize', position, { signal })
    const observer = new ResizeObserver(position)
    observer.observe($('main'))
    observer.observe($('#compose'))
    signal.addEventListener('abort', () => observer.disconnect())
    // closedby="any" would close the picker before its toggle buttons run.
    document.addEventListener('pointerdown', event => {
      if (!node.contains(event.target) && !event.target.closest('#emoji-button,#sticker-button,#gif-button')) node.close()
    }, { signal })
    node.addEventListener('keydown', event => {
      if (event.key !== 'Escape') return
      event.preventDefault()
      event.stopPropagation()
      node.close()
    }, { signal })
  } else {
    node.closedBy = 'any'
    node.showModal()
  }
  return node
}
function confirmDialog(title, text, label, run) {
  const node = modal(title)
  node.append(element('p', '', text), button(label, async () => { await run(); node.close(); await refreshState(true) }, 'danger'))
}
// Lists people from an array or a pending request.
async function peopleDialog(title, load, empty = 'No users available.') {
  const body = element('div')
  const node = modal(title)
  node.append(body)
  body.append(element('p', 'muted', 'Loading…'))
  try {
    const users = await load
    if (node.open) body.replaceChildren(...users.map(person), ...users.length ? [] : [element('p', 'muted', empty)])
  } catch (error) {
    if (node.open) body.replaceChildren(element('p', 'error', error.message))
  }
}
function safeMedia(value) {
  if (typeof value !== 'string') return ''
  try { return new URL(value).protocol === 'https:' ? value : '' } catch { return '' }
}
function initials(name) { return name.split(/\s+/u).map(word => word[0]).join('').slice(0, 2) }
function hex(color) { return '#' + color.toString(16).padStart(6, '0') }
function emojiUrl(id, animated = false, size = 48) { return `https://cdn.discordapp.com/emojis/${id}.${animated ? 'gif' : 'webp'}?size=${size}` }
function stickerImage(sticker) {
  return sticker.format_type === 3 ? '' : sticker.format_type === 4 ? `https://media.discordapp.net/stickers/${sticker.id}.gif` : `https://cdn.discordapp.com/stickers/${sticker.id}.png`
}
function image(src, className = '', alt = '') {
  const node = element('img', className)
  node.src = src
  node.alt = alt
  return node
}
function userIcon(user) {
  const node = element('span', 'avatar', initials(user.name || 'Unknown'))
  if (safeMedia(user.avatar)) {
    const picture = image(user.avatar)
    picture.loading = 'lazy'
    node.replaceChildren(picture)
  }
  return node
}
function guildIcon(node, guild) {
  if (safeMedia(guild.icon)) node.replaceChildren(image(guild.icon))
  else node.textContent = initials(guild.name)
  return node
}
// Applies a member's nickname and server avatar to a cached user.
function memberUser(user, member, guild) {
  const hash = member?.avatar
  return { ...user, name: member?.nick || user.name, ...hash && guild ? { avatar: `https://cdn.discordapp.com/guilds/${guild}/users/${user.id}/avatars/${hash}.${hash.startsWith('a_') ? 'gif' : 'png'}?size=128` } : {} }
}
function contextualUser(id, guild) {
  const user = snapshot.users[id]
  return user ? memberUser(user, guildById(guild)?.members?.[id], guild) : null
}
function dragToken(node, kind, id) {
  node.draggable = true
  node.ondragstart = event => {
    event.dataTransfer.setData('application/x-suncord', JSON.stringify({ kind, id }))
    event.dataTransfer.effectAllowed = 'copy'
  }
}
function person(user) {
  const node = element('button', 'person')
  node.append(userIcon(user), element('span', '', user.name))
  dragToken(node, 'user', user.id)
  node.onclick = () => showProfile(user)
  return node
}
function retainChildren(target, nodes) {
  const retained = new Set(nodes)
  for (const node of [...target.children]) if (!retained.has(node)) node.remove()
  nodes.forEach((node, index) => { if (target.children[index] !== node) target.insertBefore(node, target.children[index] || null) })
}
function status(message) { $('#status').textContent = message }

// The server rejects requests made for an account other than the signed-in one.
// Dialogs pass the account captured when they opened.
// Returns the response, which is 304 only when the caller sent If-None-Match.
async function apiResponse(path, { account = snapshot?.user?.id || clientState?.account || '', ...options } = {}) {
  const headers = new Headers(options.headers)
  headers.set('X-Suncord-Account', account)
  const response = await fetch('/api/' + path, { ...options, headers })
  if (!response.ok && !(response.status === 304 && headers.has('If-None-Match'))) throw new Error(
    response.status === 409 || response.status === 503 ? await response.text()
      : response.status === 401 ? 'Open the launch link printed by the local server.'
        : `Local request failed (${response.status}).`)
  return response
}
let cachedMessages
async function api(path, { account = snapshot?.user?.id || clientState?.account || '', ...options } = {}) {
  const cacheable = path.startsWith('messages/') && !options.method
  const cached = cacheable && cachedMessages?.path === path && cachedMessages.account === account ? cachedMessages : null
  const headers = new Headers(options.headers)
  if (cached?.tag) headers.set('If-None-Match', cached.tag)
  const response = await apiResponse(path, { ...options, headers, account })
  if (response.status === 304) return cached.rows
  const result = response.status === 204 ? null : await response.json()
  if (cacheable) cachedMessages = { path, account, tag: response.headers.get('ETag'), rows: result }
  return result
}
function post(path, body, account) {
  return api(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body), account })
}
function action(name, values = {}, account, media) { return post('action', { action: name, ...values, media }, account) }
function ui(command, values = {}, account) { return post('ui', { command, ...values }, account) }

// Uploads files, runs send with their ids, then discards the uploads whatever happened.
const fileLimit = 10 * 1024 * 1024
async function withUploads(picked, account, send, progress = () => {}) {
  const ids = []
  try {
    for (const file of picked) {
      if (file.size > fileLimit) throw new Error('Each file must be 10 MiB or smaller.')
      progress(file)
      ids.push((await api('files?name=' + encodeURIComponent(file.name), { method: 'POST', body: file, account })).id)
    }
    return await send(ids)
  } finally {
    await Promise.allSettled(ids.map(id => api('files/' + encodeURIComponent(id), { method: 'DELETE', account })))
  }
}

let featureRequests = { session: null, entries: new Map() }
function featureSession() {
  const session = JSON.stringify([snapshot?.user?.id || clientState?.account || '', clientState?.session ?? null])
  if (featureRequests.session !== session) featureRequests = { session, entries: new Map() }
  return featureRequests
}
// Remote lookups are shared until the account or connection changes.
function cachedFeature(command, values = {}) {
  const scope = featureSession(), key = JSON.stringify([command, values])
  if (!scope.entries.has(key)) {
    const pending = ui(command, values).catch(error => {
      if (scope.entries.get(key) === pending) scope.entries.delete(key)
      throw error
    })
    scope.entries.set(key, pending)
    if (scope.entries.size > 100) scope.entries.delete(scope.entries.keys().next().value)
  }
  return scope.entries.get(key)
}
// Returns a check that stays true while the channel, navigation and session are unchanged.
function staleGuard() {
  const id = channel, revision = request, session = featureSession()
  return () => channel === id && request === revision && session === featureSession()
}

let lookups = {}
function indexed() {
  if (lookups.snapshot !== snapshot) lookups = {
    snapshot,
    channels: new Map([...snapshot.channels, ...snapshot.dms].map(item => [item.id, item])),
    guilds: new Map(snapshot.guilds.map(guild => [guild.id, guild])),
    voice: new Map(),
  }
  return lookups
}
function channelById(id) { return indexed().channels.get(id) }
function guildById(id) { return indexed().guilds.get(id) }
function isForum(item) { return item?.kind === 15 || item?.kind === 16 }
function isVoice(item) { return item?.kind === 2 || item?.kind === 13 }
function isThread(item) { return item?.kind >= 10 && item?.kind <= 12 }
let roleOrder = { sorted: [] }
function sortedRoles() {
  if (roleOrder.roles !== features.view.roles) roleOrder = { roles: features.view.roles, sorted: [...features.view.roles || []].sort((a, b) => b.position - a.position) }
  return roleOrder.sorted
}
function roleById(id) { return features.view.roles?.find(role => role.id === id) }
function dialogOpen() { return !!document.querySelector('dialog[open],#account-menu:popover-open') }
