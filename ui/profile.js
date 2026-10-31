// User profiles and role member lists.
async function showProfile(user, server = false, sourceGuild = selectedGuild) {
  const session = featureSession(), id = user.id, guild = server ? sourceGuild : null
  const node = modal(user.name || 'Profile', 'user-profile'), body = element('div', 'profile-content')
  const active = () => node.open && node.isConnected && session === featureSession()
  const knownFriends = () => Object.hasOwn(snapshot.mutualFriends, id) ? snapshot.mutualFriends[id].map(id => snapshot.users[id]).filter(Boolean) : null
  let mutualFriends = knownFriends() || []
  node.append(body)
  const mutualControl = (label, previews, run) => {
    const control = button('', run, 'mutual-control'), icons = element('span', 'mutual-previews')
    icons.append(...previews.length ? previews.slice(0, 3).map(userIcon) : [icon('members')])
    control.append(icons, label)
    return control
  }
  // Server profiles arrive with the member's nickname, avatar, banner and colours already applied.
  const paint = data => {
    if (!active()) return
    const profile = data.user || user, member = server ? data.member : null
    const cover = element('div', 'profile-cover')
    if (safeMedia(profile.banner)) {
      cover.append(image(profile.banner))
      cover.onclick = () => imageViewer(profile.banner, 'Profile banner')
    }
    if (profile.accentColor != null) node.style.setProperty('--profile-accent', hex(profile.accentColor))
    if (profile.themeColors?.length === 2) {
      const [top, bottom] = profile.themeColors.map(hex)
      node.style.background = `linear-gradient(${top},${bottom})`
      node.style.setProperty('--profile-bg', top)
    }
    const avatar = userIcon(profile)
    if (safeMedia(profile.avatar)) {
      avatar.tabIndex = 0
      avatar.setAttribute('role', 'button')
      avatar.setAttribute('aria-label', 'View profile avatar')
      avatar.onclick = () => imageViewer(profile.avatar, profile.name)
      avatar.onkeydown = event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); avatar.click() } }
    }
    const names = element('div', 'profile-names')
    names.append(element('h3', '', member?.nick || profile.name))
    if (profile.username) names.append(element('p', '', '@' + profile.username))
    body.replaceChildren(cover, avatar, names)
    const presence = data.presence, custom = presence?.custom_status
    if (custom) {
      const bubble = element('div', 'profile-status'), content = element('span')
      if (custom.emoji?.id) bubble.append(image(emojiUrl(custom.emoji.id), 'inline-emoji', custom.emoji.name || ''), ' ')
      else if (custom.emoji?.name) bubble.append(custom.emoji.name + ' ')
      if (data.statusHtml) content.innerHTML = data.statusHtml
      else content.textContent = custom.text || ''
      bubble.append(content)
      richContent(bubble)
      body.append(bubble)
    }
    if (presence?.status) { avatar.dataset.status = presence.status; avatar.title = presence.status }
    const about = member?.bio || profile.bio
    if (data.bioHtml || about) {
      const bio = element('div', 'bio')
      if (data.bioHtml) bio.innerHTML = data.bioHtml
      else bio.append(element('p', '', about))
      richContent(bio)
      body.append(bio)
    }
    const roles = (features.view.roles || []).filter(role => member?.roles?.includes(role.id))
    if (roles.length) {
      const section = element('section', 'profile-roles')
      section.append(element('h4', 'profile-heading', 'Roles'), ...roles.map(role => {
        const chip = button('', () => roleDetails(role.id), 'role-chip')
        if (role.color) chip.style.setProperty('--role-color', hex(role.color))
        chip.title = role.name
        chip.append(element('span', '', role.name))
        return chip
      }))
      body.append(section)
    }
    const mutuals = element('div', 'profile-mutuals')
    const guilds = snapshot.guilds.filter(g => profile.mutualGuilds == null ? !!g.members?.[id] : profile.mutualGuilds.includes(g.id))
    if (guilds.length) mutuals.append(mutualControl(`${guilds.length} mutual ${guilds.length === 1 ? 'server' : 'servers'}`, guilds.map(g => ({ id: g.id, name: g.name, avatar: g.icon })), () => {
      const panel = modal('Mutual servers')
      panel.append(...guilds.map(g => button(g.name, () => { panel.close(); node.close(); showGuild(g) })))
    }))
    const friends = profile.mutualFriendsCount ?? mutualFriends.length
    if (id !== snapshot.user?.id && friends > 0) mutuals.append(mutualControl(`${friends} mutual ${friends === 1 ? 'friend' : 'friends'}`, mutualFriends, () => {
      peopleDialog('Mutual friends', knownFriends() ?? (mutualFriends.length ? mutualFriends : cachedFeature('friends', { user: id })), 'No mutual friends.')
    }))
    if (mutuals.childElementCount) body.append(mutuals)
    if (sourceGuild) body.append(button(server ? 'View global profile' : 'View server profile', () => { node.close(); showProfile(profile, !server, sourceGuild) }, 'profile-switch'))
  }
  const cached = snapshot.users[id] || user, member = guildById(guild)?.members?.[id]
  paint({ user: guild ? memberUser(cached, member, guild) : cached, member })
  let refreshError
  try { await cachedFeature('profile', { user: id, guild }) } catch (error) { refreshError = error }
  if (!active()) return
  try {
    const data = await ui('profile', { user: id, guild, cached: true })
    paint(data)
    if (refreshError && active()) body.append(element('p', 'error', refreshError.message))
    if (id !== snapshot.user?.id && data.user?.mutualFriendsCount > 0 && !knownFriends()) {
      mutualFriends = await cachedFeature('friends', { user: id })
      paint(data)
    }
  } catch (error) { if (active()) body.append(element('p', 'error', error.message)) }
}

async function roleDetails(id) {
  const panel = modal('Role members'), guild = selectedGuild, session = featureSession()
  const members = guildById(guild)?.members || {}, ids = Object.keys(members).filter(user => members[user].roles?.includes(id))
  features.role = { node: panel, guild, ids: ids.slice(0, 100), total: ids.length, name: roleById(id)?.name || 'Role members' }
  panel.addEventListener('close', () => { if (features.role?.node === panel) features.role = null }, { once: true })
  renderRoleMembers()
  try {
    const data = await ui('role', { channel, role: id })
    if (!panel.open || session !== featureSession() || features.role?.node !== panel) return
    features.role = { node: panel, guild, ids: data.ids, total: data.total, name: data.role?.name || 'Role members' }
    renderRoleMembers()
    if (data.error) panel.append(element('p', 'error', data.error))
  } catch (error) { if (panel.open && session === featureSession()) panel.append(element('p', 'error', error.message)) }
}
function renderRoleMembers() {
  const role = features.role
  if (!role?.node.isConnected) { features.role = null; return }
  role.node.querySelector('h2').textContent = `${role.name} — ${role.total}`
  let body = role.node.querySelector('.role-members')
  if (!body) {
    body = element('div', 'role-members')
    for (const item of role.node.querySelectorAll(':scope > p')) item.remove()
    role.node.append(body)
  }
  const members = guildById(role.guild)?.members || {}
  const signature = JSON.stringify(role.ids.map(id => [snapshot.users[id], members[id]]))
  if (body.dataset.signature === signature) return
  body.dataset.signature = signature
  body.replaceChildren(...role.ids.map(id => {
    if (!snapshot.users[id]) return element('p', 'muted', 'Loading member…')
    const user = memberUser(snapshot.users[id], members[id], role.guild), node = person(user)
    node.onclick = () => showProfile(user, true, role.guild)
    return node
  }))
  if (!role.ids.length) body.append(element('p', 'muted', 'No members.'))
}
