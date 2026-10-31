// The side panel shows members, pinned messages or the inbox.
function showSidePanel(kind) {
  const messages = kind === 'pins' || kind === 'inbox'
  $('#members').hidden = kind !== 'members'
  $('#message-panel').hidden = !messages
  if (!messages) features.panel = null
  app.classList.toggle('members-open', !!kind)
  for (const [id, name] of [['#toggle-members', 'members'], ['#pins-button', 'pins'], ['#inbox-button', 'inbox']]) $(id).setAttribute('aria-pressed', String(kind === name))
  $('#toggle-members').setAttribute('aria-label', kind === 'members' ? 'Hide members' : 'Show members')
}

// Member pages load in hundreds. The gateway subscription follows the visible page.
async function refreshMembers(append = false, visibleIndex = null) {
  if (!channel) return
  const id = channel, revision = request, session = featureSession(), current = staleGuard()
  if (features.memberSubscription?.id !== id || features.memberSubscription.revision !== revision) features.memberSubscription = { id, revision, index: 0, loadIndex: null }
  const subscription = features.memberSubscription
  if (visibleIndex != null) subscription.index = Math.floor(visibleIndex / 100) * 100
  if (append) subscription.index = subscription.loadIndex = features.memberIndex + 100
  const busy = features.membersBusy
  if (busy?.id === id && busy.revision === revision && busy.session === session) return
  const pending = { id, revision, session }
  features.membersBusy = pending
  const subscribedIndex = subscription.index, index = subscription.loadIndex ?? features.memberRefreshIndex
  try {
    const data = await ui('members', { channel: id, index, subscribedIndex })
    if (!current()) return
    const lastPage = Math.max(0, Math.ceil(data.total / 100) - 1) * 100
    features.memberTotal = data.total
    subscription.index = Math.min(subscription.index, lastPage)
    if (subscription.loadIndex === data.index || subscription.loadIndex > lastPage) subscription.loadIndex = null
    features.memberIndex = Math.min(Math.max(features.memberIndex, data.index), lastPage)
    features.memberRefreshIndex = data.index >= features.memberIndex ? 0 : data.index + 100
    features.memberPages.set(data.index, data.entries)
    for (const [page, entries] of features.memberPages) {
      if (page > lastPage) features.memberPages.delete(page)
      else if (page + entries.length > data.total) features.memberPages.set(page, entries.slice(0, Math.max(0, data.total - page)))
    }
    renderMembers()
  } catch (error) {
    if (current()) status(error.message)
  } finally {
    if (features.membersBusy === pending) {
      features.membersBusy = false
      if (current() && (subscription.index !== subscribedIndex || subscription.loadIndex != null && subscription.loadIndex !== index)) refreshMembers()
    }
  }
}
function memberHeading(entry) {
  const key = 'members:' + entry.group
  const heading = button('', () => {
    features.collapsed.has(key) ? features.collapsed.delete(key) : features.collapsed.add(key)
    renderMembers()
  }, 'category member-heading')
  const name = roleById(entry.group)?.name || { online: 'Online', offline: 'Offline' }[entry.group] || 'Members'
  heading.append(element('span', 'scroll-label', `${name} — ${entry.count}`), icon('chevron'))
  heading.dataset.group = entry.group
  return heading
}
function memberRow({ user, member, presence }) {
  const node = person(user), roles = sortedRoles().filter(role => member?.roles?.includes(role.id)), colored = roles.find(role => role.color)
  node.classList.add('member')
  node.dataset.status = presence?.status || 'offline'
  node.onclick = () => showProfile(user, true)
  if (presence?.custom_status?.text) node.append(element('small', 'member-detail', presence.custom_status.text))
  if (colored) node.style.color = hex(colored.color)
  if (roles[0]) node.append(element('small', 'member-detail', roles[0].name))
  return node
}
function renderMembers() {
  const target = $('#member-list'), existing = new Map([...target.children].filter(node => node.dataset.key).map(node => [node.dataset.key, node]))
  let position = 0, hidden = false
  const nodes = [...features.memberPages].sort((a, b) => a[0] - b[0]).flatMap(([page, entries]) => entries.map((entry, offset) => {
    const key = entry.group ? 'group:' + entry.group : entry.user ? 'user:' + entry.user.id : 'loading:' + position
    const signature = JSON.stringify([entry, features.view.roles])
    position++
    let node = existing.get(key)
    if (node?.dataset.signature !== signature) {
      node = entry.group ? memberHeading(entry) : entry.user ? memberRow(entry) : element('p', 'muted', 'Loading…')
      node.dataset.key = key
      node.dataset.signature = signature
    }
    if (entry.user) node.dataset.memberIndex = page + offset
    return node
  }))
  for (const node of nodes) {
    if (node.dataset.group) {
      hidden = features.collapsed.has('members:' + node.dataset.group)
      node.setAttribute('aria-expanded', String(!hidden))
    } else {
      node.classList.toggle('collapsed', hidden)
      node.inert = hidden
      node.setAttribute('aria-hidden', String(hidden))
    }
  }
  retainChildren(target, nodes)
  if (features.memberIndex + (features.memberPages.get(features.memberIndex)?.length || 0) < features.memberTotal) target.append(button('Load more members', () => refreshMembers(true)))
}
$('#members').onscroll = () => {
  const top = $('#members').getBoundingClientRect().top + 12
  const first = [...$('#member-list').querySelectorAll('.member:not(.collapsed)')].find(node => node.getBoundingClientRect().bottom > top)
  const index = first ? Math.floor(Number(first.dataset.memberIndex) / 100) * 100 : 0
  if (index !== features.memberSubscription?.index) refreshMembers(false, index)
}
$('#toggle-members').onclick = () => {
  const opening = $('#members').hidden
  showSidePanel(opening ? 'members' : null)
  if (opening) refreshMembers()
  else refreshFeatures().catch(error => status(error.message))
}

function panelRow(message, kind) {
  const { guild, user, role } = messageAuthor(message), origin = channelById(message.channelId)
  const row = element('article', 'panel-message'), body = element('div'), header = element('header')
  const author = element('strong', 'author', user.name), time = element('time', 'message-time', messageTimestamp(new Date(message.timestamp)))
  if (role) author.style.color = hex(role.color)
  time.dateTime = message.timestamp
  header.append(author, time)
  body.append(header)
  if (kind === 'inbox' && origin) body.append(element('small', 'panel-context', [guildById(guild)?.name, (guild ? '#' : '') + origin.name].filter(Boolean).join(' · ')))
  body.append(message.html || message.content ? renderedContent(message.html, message.content, guild) : element('p', 'muted', 'Attachment'))
  row.append(userIcon(user), body)
  row.tabIndex = 0
  row.onclick = event => { if (!event.target.closest('a, button, .spoiler')) openMessage(message.channelId, message.id).catch(error => status(error.message)) }
  row.onkeydown = event => { if (event.key === 'Enter' && event.target === row) row.click() }
  return row
}
// Panels load when opened. A reload rebuilds the rows only when the first page changed.
async function messagePanel(kind, { more = false, reload = false } = {}) {
  if (!more && !reload) {
    showSidePanel(kind)
    const node = $('#message-panel')
    node.replaceChildren(element('h2', '', kind === 'pins' ? 'Pinned messages' : 'Mentions'), element('div', 'panel-rows'))
    features.panel = { node, kind, cursor: null, ids: new Set(), page: null, mentions: JSON.stringify(snapshot.mentionCounts) }
  }
  const panel = features.panel
  if (panel?.kind !== kind) return
  try {
    const data = await ui('panel', { kind, channel: channel || null, cursor: more ? panel.cursor : null })
    if (features.panel !== panel) return
    const body = panel.node.querySelector('.panel-rows')
    if (!more) {
      const page = JSON.stringify(data.rows)
      if (page === panel.page) return
      panel.page = page
      panel.ids.clear()
      body.replaceChildren()
    }
    panel.node.querySelector('.panel-more, .error')?.remove()
    for (const message of data.rows) {
      if (panel.ids.has(message.id)) continue
      panel.ids.add(message.id)
      body.append(panelRow(message, kind))
    }
    if (!body.children.length) {
      const empty = element('div', 'panel-empty')
      empty.append(icon(kind === 'pins' ? 'pin' : 'inbox'), element('p', '', kind === 'pins' ? 'No pinned messages in this channel' : 'No recent mentions'))
      body.append(empty)
    }
    panel.cursor = data.cursor
    if (data.cursor) panel.node.append(button('Load more', () => messagePanel(kind, { more: true }), 'panel-more'))
  } catch (error) {
    if (features.panel === panel) {
      panel.node.querySelector('.error')?.remove()
      panel.node.append(element('p', 'error', error.message))
    }
  }
}
$('#pins-button').onclick = () => features.panel?.kind === 'pins' ? showSidePanel(null) : messagePanel('pins')
$('#inbox-button').onclick = () => features.panel?.kind === 'inbox' ? showSidePanel(null) : messagePanel('inbox')
