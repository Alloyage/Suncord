// The server rail, the channel list, unread markers and forum posts.
function scrollingLabel(text, control, className = '') {
  const label = element('span', 'scroll-label ' + className), content = element('span', 'scroll-label-text', text)
  label.append(content)
  let animation
  const reset = () => { animation?.cancel(); animation = null; content.style.transform = '' }
  control.addEventListener('pointerenter', () => {
    reset()
    const travel = content.scrollWidth - label.clientWidth
    if (travel <= 0 || matchMedia('(prefers-reduced-motion: reduce)').matches) return
    const duration = Math.max(1400, travel * 30), total = 700 + duration + 1400 + 160
    animation = content.animate([
      { transform: 'translateX(0)', offset: 0 },
      { transform: 'translateX(0)', offset: 700 / total },
      { transform: `translateX(${-travel}px)`, offset: (700 + duration) / total },
      { transform: `translateX(${-travel}px)`, offset: (700 + duration + 1400) / total },
      { transform: 'translateX(0)', offset: 1 },
    ], { duration: total, iterations: Infinity })
  })
  control.addEventListener('pointerleave', () => {
    const transform = getComputedStyle(content).transform
    reset()
    if (transform !== 'none') content.animate([{ transform }, { transform: 'translateX(0)' }], { duration: 160 })
  })
  control.addEventListener('pointerdown', reset)
  return label
}

function renderServers() {
  const signature = JSON.stringify([snapshot.user?.id, snapshot.guilds.map(g => [g.id, g.name, g.icon]), snapshot.folders])
  if (signature === structure) return
  structure = signature
  const rail = $('#servers')
  const servers = new Map([...rail.querySelectorAll('.server-entry')].map(node => [node.dataset.id, node]))
  const folders = new Map([...rail.querySelectorAll('.guild-folder')].map(node => [node.dataset.id, node]))
  const serverButton = guild => {
    let node = servers.get(guild.id)
    if (!node) {
      node = button('', () => showGuild(guildById(guild.id)), 'server-entry')
      node.dataset.id = guild.id
      dragToken(node, 'guild', guild.id)
    }
    const signature = JSON.stringify([guild.name, guild.icon])
    if (node.dataset.signature !== signature) {
      node.dataset.signature = signature
      node.title = guild.name
      node.setAttribute('aria-label', guild.name)
      guildIcon(node, guild)
    }
    return node
  }
  const folderOf = new Map()
  for (const folder of snapshot.folders || []) {
    const members = folder.guildIds.map(guildById).filter(Boolean)
    if (!members.length) continue
    let node = folders.get(folder.id)
    if (!node) {
      node = element('details', 'guild-folder')
      node.name = 'server-folders'
      node.dataset.id = folder.id
      node.append(element('summary'), element('div'))
      node.ontoggle = () => renderUnread()
    }
    const summary = node.firstElementChild, signature = JSON.stringify([folder.name, folder.color, members.slice(0, 4).map(g => [g.name, g.icon])])
    if (summary.dataset.signature !== signature) {
      summary.dataset.signature = signature
      summary.title = folder.name || 'Server folder'
      summary.style.color = folder.color == null ? '' : hex(folder.color)
      const previews = element('span', 'folder-previews')
      previews.append(...members.slice(0, 4).map(guild => guildIcon(element('span', 'folder-preview'), guild)))
      summary.replaceChildren(previews, icon('folder'))
    }
    retainChildren(node.lastElementChild, members.map(serverButton))
    for (const guild of members) folderOf.set(guild.id, node)
  }
  const nodes = new Set(snapshot.guilds.map(guild => folderOf.get(guild.id) || serverButton(guild)))
  retainChildren(rail, [...rail.children].filter(node => !node.matches('.server-entry,.guild-folder')).concat([...nodes]))
}
function refreshServerStatus() {
  const states = new Map(), empty = () => ({ unread: false, mentions: 0, voice: false })
  const own = callActive() && channelById(clientState.media.channel_id)?.guildId
  for (const [id, users] of Object.entries(snapshot.voice || {})) {
    const key = channelById(id)?.guildId
    if (key && users.length) states.set(key, { ...states.get(key) || empty(), voice: true })
  }
  for (const id in snapshot.calls || {}) states.set(null, { ...states.get(null) || empty(), voice: true })
  for (const item of [...snapshot.channels, ...snapshot.dms]) {
    if (!snapshot.readable.has(item.id)) continue
    const key = item.guildId || null, state = states.get(key) || empty(), read = snapshot.lastRead[item.id]
    state.unread ||= !!(item.lastMessageId && read && BigInt(item.lastMessageId) > BigInt(read))
    state.mentions += snapshot.mentionCounts[item.id] || 0
    states.set(key, state)
  }
  const decorate = (node, state, label, joined = false) => {
    node.classList.toggle('unread', state.unread)
    let speaker = node.querySelector(':scope > .server-voice')
    if (state.voice && !speaker) { speaker = element('span', 'server-voice'); speaker.append(icon('voice')); node.append(speaker) }
    if (speaker) { speaker.hidden = !state.voice; speaker.classList.toggle('joined', joined) }
    let badge = node.querySelector(':scope > .server-mentions')
    if (state.mentions && !badge) { badge = element('span', 'server-mentions'); node.append(badge) }
    if (badge) {
      badge.hidden = !state.mentions
      badge.textContent = state.mentions > 99 ? '99+' : String(state.mentions)
      badge.setAttribute('aria-label', state.mentions + ' mentions')
    }
    node.setAttribute('aria-label', label + (state.voice ? ' · people in voice' : '') + (state.mentions ? ' · ' + state.mentions + ' mentions' : state.unread ? ' · unread' : ''))
  }
  for (const node of $('#servers').querySelectorAll('.server-entry')) {
    const guild = guildById(node.dataset.id)
    if (guild) decorate(node, states.get(guild.id) || empty(), guild.name, guild.id === own)
  }
  for (const node of $('#servers').querySelectorAll('.guild-folder')) {
    const folder = snapshot.folders?.find(folder => folder.id === node.dataset.id)
    if (!folder) continue
    const state = empty()
    for (const id of folder.guildIds) {
      const guild = states.get(id)
      if (guild) { state.unread ||= guild.unread; state.mentions += guild.mentions; state.voice ||= guild.voice }
    }
    node.classList.toggle('contains-selected', !node.open && folder.guildIds.includes(selectedGuild))
    node.classList.toggle('unread', state.unread)
    decorate(node.querySelector('summary'), { unread: state.unread, mentions: node.open ? 0 : state.mentions, voice: !node.open && state.voice }, folder.name || 'Server folder', !node.open && folder.guildIds.includes(own))
  }
  decorate($('#servers .sun'), states.get(null) || empty(), 'Direct messages')
}
function renderUnread() {
  for (const node of $('#channels').querySelectorAll('[data-id]')) {
    const id = node.dataset.id, item = channelById(id), read = snapshot.lastRead[id]
    node.classList.toggle('unread', !!(item?.lastMessageId && read && BigInt(item.lastMessageId) > BigInt(read)))
    let badge = node.querySelector('.unread-count')
    if (!badge) { badge = element('span', 'unread-count'); node.append(badge) }
    const count = snapshot.mentionCounts[id] || 0
    badge.textContent = count ? String(count) : ''
    badge.hidden = !count
  }
  refreshServerStatus()
}
function renderChannelSelection(item) {
  if (item) {
    $('#channel-name').textContent = item.name
    draft.placeholder = 'Message ' + item.name
    draft.setAttribute('aria-label', 'Message ' + item.name)
  }
  for (const node of $('#channels').querySelectorAll('.channel')) {
    node.classList.toggle('active', node.dataset.id === channel)
    node.setAttribute('aria-current', node.dataset.id === channel ? 'page' : 'false')
  }
}

function showGuild(guild, choose = true) {
  selectedGuild = guild?.id ?? null
  $('#servers .sun').classList.toggle('selected', !guild)
  $('#workspace-name').textContent = guild?.name || 'Direct messages'
  const banner = safeMedia(guild?.banner)
  if ($('#workspace-banner')?.getAttribute('src') !== (banner || undefined)) $('#workspace-banner')?.remove()
  if (banner && !$('#workspace-banner')) {
    const node = image(banner)
    node.id = 'workspace-banner'
    $('#workspace').prepend(node)
  }
  $('#workspace').classList.toggle('has-banner', !!banner)
  $('#channel-symbol').textContent = guild ? '#' : '@'
  const readable = item => snapshot.readable.has(item.id)
  const channels = guild
    ? snapshot.channels.filter(item => item.guildId === guild.id && [0, 2, 5, 10, 11, 12, 13, 15, 16].includes(item.kind) && (features.prefs.showInaccessibleChannels || readable(item))).sort((a, b) => a.position - b.position)
    : snapshot.dms
  renderChannels(guild, channels)
  renderChannelSelection(channels.find(item => item.id === channel))
  renderUnread()
  for (const node of $('#servers').querySelectorAll('[data-id]')) node.classList.toggle('selected', node.dataset.id === guild?.id)
  const selectable = channels.filter(readable), has = id => selectable.some(item => item.id === id)
  if (choose && selectable.length) {
    const visited = snapshot.visitedChannels[guild?.id || '']
    const next = has(channel) ? channel : has(visited) ? visited : has(snapshot.activeChannelId) ? snapshot.activeChannelId : selectable[0].id
    if (channel !== next || !document.documentElement.dataset.ready && !selectingChannel) selectChannel(next)
  } else if (choose || channel && !has(channel)) {
    leaveChannel(null)
    renderChannelSelection()
    $('#channel-name').textContent = guild?.name || 'Direct messages'
  }
}
function renderChannels(guild, channels) {
  const sidebar = $('#channels'), readable = item => snapshot.readable.has(item.id)
  const categories = snapshot.channels.filter(c => c.guildId === guild?.id && c.kind === 4).sort((a, b) => a.position - b.position)
  const signature = JSON.stringify([snapshot.user?.id, selectedGuild, channels.map(({ lastMessageId, ...item }) => item), categories, snapshot.pinnedDms, channels.map(readable), clientState?.ready, !guild && channels.map(c => c.recipientIds?.map(id => snapshot.users[id])), guild && channels.filter(isVoice).map(c => voiceUsers(c.id, guild.id)), !guild && channels.map(c => callOngoing(c.id))])
  if (sidebar.dataset.signature === signature) return
  sidebar.dataset.signature = signature
  const existing = new Map([...sidebar.querySelectorAll('[data-sidebar-key]')].map(node => [node.dataset.sidebarKey, node]))
  const nodes = [], pinned = new Set(snapshot.pinnedDms)
  const drawChannel = (item, depth = 0) => {
    const key = 'channel:' + item.id, voice = isVoice(item)
    const people = guild && voice ? voiceUsers(item.id, guild.id) : []
    const peerId = !guild && item.kind === 1 ? item.recipientIds?.find(id => id !== snapshot.user?.id) : null
    const peer = peerId && snapshot.users[peerId]
    const signature = JSON.stringify([item.name, item.kind, item.icon, depth, readable(item), clientState?.ready, peer, pinned.has(item.id), people, !guild && callOngoing(item.id)])
    let row = existing.get(key)
    if (row?.dataset.signature === signature) return row
    // A kind change can change a row's controls. Ordinary updates retain them.
    if (row && (row.classList.contains('voice-row') !== voice || row.classList.contains('dm-row') !== !guild)) row = null
    let node = row?.matches('.channel') ? row : row?.querySelector('.channel')
    if (!node) {
      node = button('', () => voice ? joinVoice(item.id) : selectChannel(item.id), 'channel')
      node.dataset.id = item.id
      dragToken(node, 'channel', item.id)
      node.append(element('span'), scrollingLabel(item.name, node, 'channel-label'))
      if (!guild) {
        row = element('div', 'dm-row')
        row.append(node, iconLabel(button('', async () => { await ui('pinDm', { channel: item.id }); await refreshState(true) }, 'dm-pin'), 'pin', 'Pin or unpin conversation'))
      } else if (voice) {
        row = element('div', 'voice-row')
        const chat = button('', () => selectChannel(item.id), 'voice-chat')
        chat.append(icon('chat'))
        row.append(node, chat)
      } else row = node
    }
    node.style.paddingLeft = 8 + depth * 12 + 'px'
    node.querySelector('.scroll-label-text').textContent = item.name
    node.firstElementChild.replaceWith(guild ? icon(voice ? 'voice' : isForum(item) ? 'forum' : isThread(item) ? 'thread' : item.kind === 5 ? 'announcement' : 'channel') : userIcon(peer || { id: item.id, name: item.name, avatar: item.icon }))
    node.classList.toggle('inaccessible', !readable(item))
    node.title = readable(item) ? '' : 'You cannot read this channel'
    if (voice) {
      node.dataset.voice = item.id
      node.classList.add('voice')
      node.disabled = !clientState?.ready
      const chat = row.querySelector('.voice-chat')
      chat.setAttribute('aria-label', 'Open ' + item.name + ' text chat')
      chat.title = 'Open text chat'
      let list = row.querySelector('.voice-users')
      if (!list) { list = element('div', 'voice-users'); row.append(list) }
      list.replaceChildren(...people.map(state => {
        const node = person(state.user)
        node.lastElementChild.className = 'voice-name'
        node.append(...voiceTags(state))
        voicePerson(node, state)
        return node
      }))
    }
    if (!guild) {
      row.querySelector('.dm-pin').classList.toggle('pinned', pinned.has(item.id))
      node.querySelector('.dm-call')?.remove()
      if (callOngoing(item.id)) node.querySelector('.channel-label').after(Object.assign(icon('voice'), { className: 'icon dm-call', title: 'Call in progress' }))
      let profile = row.querySelector('.dm-avatar')
      if (peer) {
        if (!profile) { profile = button('', () => showProfile(snapshot.users[row.dataset.peer]), 'dm-avatar'); row.append(profile) }
        row.dataset.peer = peer.id
        profile.setAttribute('aria-label', 'View ' + peer.name + ' profile')
        profile.title = 'View ' + peer.name + ' profile'
        dragToken(profile, 'user', peer.id)
      } else profile?.remove()
    }
    row.dataset.sidebarKey = key
    row.dataset.signature = signature
    return row
  }
  const drawGroup = (id, name, children) => {
    if (!children.length) return
    const key = 'category:' + id
    let group = existing.get(key)
    if (!group) {
      group = element('details', 'channel-group')
      group.dataset.sidebarKey = key
      group.open = !features.collapsed.has(key)
      group.ontoggle = () => group.open ? features.collapsed.delete(key) : features.collapsed.add(key)
      const heading = element('summary', 'category')
      heading.append(scrollingLabel(name, heading), icon('chevron'))
      group.append(heading, element('div'))
    }
    group.querySelector('.scroll-label-text').textContent = name
    retainChildren(group.lastElementChild, children)
    nodes.push(group)
  }
  if (guild) {
    const byParent = Map.groupBy(channels, item => item.parentId)
    const categoryIds = new Set(categories.map(c => c.id)), channelIds = new Set(channels.map(c => c.id))
    const draw = items => items.flatMap(item => [drawChannel(item), ...(byParent.get(item.id) || []).map(thread => drawChannel(thread, 1))])
    for (const voice of [false, true]) {
      nodes.push(...draw(channels.filter(c => isVoice(c) === voice && !channelIds.has(c.parentId) && !categoryIds.has(c.parentId))))
      for (const group of categories) drawGroup(group.id + (voice ? '-voice' : ''), group.name, draw((byParent.get(group.id) || []).filter(c => isVoice(c) === voice)))
    }
  } else {
    drawGroup('pinned-dms', 'Pinned', channels.filter(c => pinned.has(c.id)).map(c => drawChannel(c)))
    drawGroup('direct-messages', 'Direct messages', channels.filter(c => !pinned.has(c.id)).map(c => drawChannel(c)))
  }
  retainChildren(sidebar, nodes)
}

function renderForumPosts() {
  const item = channelById(channel)
  if (item) $('#channel-name').textContent = item.name
  const threads = snapshot.channels.filter(c => c.parentId === channel && isThread(c))
  const signature = JSON.stringify([channel, threads.map(c => [c.id, c.name])])
  if (list.dataset.forumSignature === signature && list.querySelector('.forum-post,.forum-empty')) return
  list.dataset.forumSignature = signature
  const previous = new Map([...list.querySelectorAll('.forum-post')].map(node => [node.dataset.id, node]))
  const nodes = threads.map(thread => {
    let node = previous.get(thread.id)
    if (!node) { node = button('', () => selectChannel(thread.id), 'forum-post'); node.dataset.id = thread.id }
    if (node.textContent !== thread.name) node.textContent = thread.name
    return node
  })
  retainChildren(list, nodes.length ? nodes : [element('p', 'muted forum-empty', 'No cached posts in this channel.')])
}
function openForum(item) {
  leaveChannel(item.id)
  following = true
  renderChannelSelection(item)
  renderForumPosts()
  setComposer(true)
  recordLocation()
  refreshFeatures().catch(error => status(error.message))
  document.documentElement.dataset.ready = 'true'
}

// Both side panels resize from their inner edge. Preferences keep the wanted width and the window clamps it.
const resizers = [
  { handle: $('#resize'), key: 'channelPanelWidth', property: '--sidebar', min: 180, max: 420, step: 10, room: () => innerWidth - 450, width: x => x - $('#sidebar').getBoundingClientRect().left },
  { handle: $('#panel-resize'), key: 'sidePanelWidth', property: '--panel', min: 220, max: 520, step: -10, room: () => innerWidth - 470 - Number(resizers[0].handle.getAttribute('aria-valuenow')), width: x => app.getBoundingClientRect().right - 12 - x },
]
function panelWidths() {
  for (const { handle, key, property, min, max, room } of resizers) {
    const maximum = Math.max(min, Math.min(max, room()))
    const value = Math.round(Math.max(min, Math.min(maximum, features.prefs[key])))
    document.documentElement.style.setProperty(property, value + 'px')
    handle.setAttribute('aria-valuemax', String(maximum))
    handle.setAttribute('aria-valuenow', String(value))
  }
}
addEventListener('resize', panelWidths)
for (const { handle, key, step, width } of resizers) {
  const save = async () => {
    features.prefs[key] = Number(handle.getAttribute('aria-valuenow'))
    try { await ui('preferences', { value: features.prefs }) } catch (error) { status(error.message) }
  }
  handle.onpointerdown = event => { handle.setPointerCapture(event.pointerId); event.preventDefault() }
  handle.onpointermove = event => {
    if (!handle.hasPointerCapture(event.pointerId)) return
    features.prefs[key] = width(event.clientX)
    panelWidths()
  }
  handle.onpointerup = save
  handle.onkeydown = event => {
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
    event.preventDefault()
    features.prefs[key] = Number(handle.getAttribute('aria-valuenow')) + (event.key === 'ArrowLeft' ? -step : step)
    panelWidths()
    save()
  }
}
$('#servers .sun').onclick = () => {
  $('#servers .sun img').animate([{ rotate: '0deg' }, { rotate: '-360deg' }], { duration: 400, easing: 'cubic-bezier(.25,.1,.25,1)' })
  showGuild(null)
}
