// The emoji, sticker and GIF picker, which also adds reactions.
function unicodeEmojis() {
  features.emoji ||= fetch('/emojis.tsv').then(response => response.text()).then(text => {
    let group = ''
    const entries = []
    for (const line of text.split('\n')) {
      if (line.startsWith('#')) { group = line.slice(1); continue }
      const [value, name, ...aliases] = line.split('\t')
      if (value && name) entries.push({ id: value, value, name, aliases: aliases.join(' '), group, favoriteKind: 'emoji', enabled: true })
    }
    return entries
  })
  return features.emoji
}
async function expressionPicker(tab = 'emoji', reaction = null) {
  features.picker?.close()
  const node = modal(reaction ? 'Add reaction' : 'Expressions', 'expression-picker', true), current = staleGuard()
  features.picker = node
  node.addEventListener('close', () => { if (features.picker === node) features.picker = null }, { once: true })
  const active = () => node.open && node.isConnected && features.picker === node && current()
  node.dataset.tab = tab
  const rail = element('nav', 'picker-groups'), tabs = element('div', 'picker-tabs'), search = element('input'), group = element('select'), grid = element('div', 'expression-grid'), detail = element('p', 'muted picker-detail')
  rail.setAttribute('aria-label', 'Expression groups')
  search.type = 'search'
  search.placeholder = tab === 'gif' ? 'Search GIFs' : tab === 'sticker' ? 'Find the perfect sticker' : 'Find the perfect emoji'
  search.setAttribute('aria-label', 'Search expressions')
  group.setAttribute('aria-label', 'Expression group')
  group.classList.add('sr-only')
  let items = [], limit = 120, generation = 0, busy = false, failure = '', dragging = null, total = 0, timer
  for (const [id, label] of [['emoji', 'Emoji'], ['gif', 'GIFs'], ['sticker', 'Stickers']]) if (!reaction || id === 'emoji') {
    const control = button(label, () => expressionPicker(id, reaction))
    control.classList.toggle('selected', tab === id)
    tabs.append(control)
  }
  const favoriteKeys = () => tab === 'sticker' ? snapshot.favoriteStickers : snapshot.favoriteEmojis
  const reordering = () => dragging?.key && dragging.holder.dataset.group === 'favorites'
  const matches = (item, key) => item.favoriteKind === 'sticker' ? key === item.id : [item.id, ...(item.aliases || '').split(' ')].includes(key.replace(/^:|:$/gu, ''))
  const favoriteKey = item => favoriteKeys().find(key => matches(item, key))
  const redraw = () => { const scroll = grid.scrollTop; draw(); grid.scrollTop = scroll }
  grid.ondragover = event => {
    if (!reordering()) return
    event.preventDefault()
    event.stopPropagation()
    event.dataTransfer.dropEffect = 'move'
  }
  grid.ondrop = async event => {
    if (!reordering()) return
    event.preventDefault()
    event.stopPropagation()
    dragging.dropped = true
    const { key, holder } = dragging, next = holder.nextElementSibling?.dataset.group === 'favorites' ? holder.nextElementSibling.dataset.key : null
    const rest = favoriteKeys().filter(other => other !== key), index = next ? rest.indexOf(next) : rest.length
    if (index === favoriteKeys().indexOf(key)) return
    try {
      await ui('favorite', { kind: tab === 'sticker' ? 'sticker' : 'emoji', key, index })
      await refreshState(true)
    } catch (error) { failure = error.message }
    if (active()) redraw()
  }
  const visible = () => {
    const query = search.value.toLowerCase()
    if (tab === 'gif') return items
    const matching = groups.flatMap(group => items.filter(item => item.group === group.id)).filter(item => query.split(/\s+/u).every(word => `${item.name} ${item.aliases || ''} ${guildById(item.group)?.name || ''}`.toLowerCase().includes(word)))
    if (query) return matching
    const favorites = favoriteKeys().map(key => matching.find(item => matches(item, key))).filter(Boolean).map(item => ({ ...item, group: 'favorites' }))
    const frequent = tab === 'emoji' ? snapshot.frequentEmojis.map(key => matching.find(item => matches(item, key))).filter(Boolean).slice(0, 42).map(item => ({ ...item, group: 'frequent' })) : []
    return [...favorites, ...frequent, ...matching]
  }
  const choose = async item => {
    if (reaction) await ui('reaction', { channel: reaction.channelId, message: reaction.id, emoji: item.emoji || { id: null, name: item.value }, remove: false })
    else if (tab === 'sticker' && item.asLink) await prepareSticker(item.id, item.name, active)
    else if (item.asLink) insertDraft(item.image)
    else if (tab === 'sticker') chooseSticker(item.id, item.name)
    else if (tab === 'gif') {
      const before = draft.value.slice(0, draft.selectionStart), after = draft.value.slice(draft.selectionEnd)
      insertDraft(`${before && !/\s$/u.test(before) ? ' ' : ''}${item.value}${after && !/^\s/u.test(after) ? ' ' : ''}`)
    } else {
      ui('emojiUsed', { key: (item.aliases || '').split(' ')[0] || item.id }).then(() => refreshState(true)).catch(() => {})
      insertDraft(item.value)
    }
    node.close()
    if (reaction) await refreshState(true)
  }
  const tile = item => {
    const holder = element('div', 'expression-tile'), control = button('', () => choose(item), 'expression')
    holder.dataset.group = item.group
    control.title = item.name + (item.asLink ? ' · Insert as a link' : item.enabled ? '' : ' · Unavailable')
    control.disabled = !item.enabled
    control.setAttribute('aria-label', item.name)
    if (item.image) {
      const picture = image(safeMedia(item.image))
      picture.loading = 'lazy'
      picture.draggable = false
      picture.onerror = () => { control.classList.add('preview-unavailable'); control.replaceChildren(element('span', '', item.name)) }
      control.append(picture)
    } else {
      control.textContent = tab === 'sticker' ? item.name : item.value
      if (tab === 'sticker') control.classList.add('preview-unavailable')
    }
    holder.append(control)
    if (tab !== 'gif') {
      const pinned = !!favoriteKey(item)
      const pin = button(pinned ? '★' : '☆', async () => {
        const scroll = grid.scrollTop
        try {
          await ui('favorite', { kind: item.favoriteKind, key: favoriteKey(item) || (item.favoriteKind === 'sticker' ? item.id : (item.aliases || '').split(' ')[0] || item.id) })
          await refreshState(true)
        } catch (error) { failure = error.message }
        draw()
        grid.scrollTop = scroll
        const replacement = [...grid.querySelectorAll('.expression-pin')].find(other => other.dataset.id === item.id && other.parentElement.dataset.group === item.group)
        ;(replacement || search).focus()
      }, 'expression-pin')
      pin.dataset.id = item.id
      pin.title = `${pinned ? 'Unpin' : 'Pin'} ${item.name}`
      pin.setAttribute('aria-label', pin.title)
      pin.setAttribute('aria-pressed', String(pinned))
      holder.append(pin)
    }
    control.draggable = item.enabled
    control.ondragstart = event => {
      dragging = { holder, key: favoriteKey(item), dropped: false }
      grid.dataset.dragging = ''
      // The browser snapshots the hovered tile, so drag a bare copy of its content instead.
      const ghost = element('div', 'expression-ghost')
      ghost.append(control.firstChild.cloneNode(true))
      if (!item.image) ghost.textContent = control.textContent
      ghost.style.width = control.offsetWidth + 'px'
      ghost.style.height = control.offsetHeight + 'px'
      document.body.append(ghost)
      event.dataTransfer.setDragImage(ghost, control.offsetWidth / 2, control.offsetHeight / 2)
      requestAnimationFrame(() => ghost.remove())
      const value = item.asLink ? item.image : item.value
      event.dataTransfer.effectAllowed = 'copyMove'
      event.dataTransfer.setData('text/plain', value)
      event.dataTransfer.setData('application/x-suncord', JSON.stringify({ kind: tab, id: item.id, name: item.name, value, asLink: item.asLink }))
      requestAnimationFrame(() => { if (dragging?.holder === holder) holder.classList.add('dragging') })
    }
    control.ondragend = () => {
      const dropped = dragging?.dropped
      dragging = null
      holder.classList.remove('dragging')
      delete grid.dataset.dragging
      if (!dropped) redraw()
    }
    if (item.group === 'favorites') holder.dataset.key = favoriteKey(item)
    holder.ondragover = event => {
      if (!reordering() || item.group !== 'favorites') return
      event.preventDefault()
      event.stopPropagation()
      event.dataTransfer.dropEffect = 'move'
      if (holder === dragging.holder) return
      const bounds = holder.getBoundingClientRect(), after = event.clientX > bounds.left + bounds.width / 2
      if (after ? holder.nextElementSibling !== dragging.holder : holder.previousElementSibling !== dragging.holder) holder[after ? 'after' : 'before'](dragging.holder)
    }
    return holder
  }
  const draw = () => {
    const filtered = visible(), children = []
    total = filtered.length
    for (const id of ['favorites', 'frequent']) {
      const control = rail.querySelector(`[data-group=${id}]`)
      if (control) control.hidden = !filtered.some(item => item.group === id)
    }
    let lastGroup
    for (const item of filtered.slice(0, limit)) {
      if (item.group !== lastGroup && tab !== 'gif') {
        const heading = element('h3', 'picker-group-heading', groups.find(group => group.id === item.group)?.name || item.group)
        heading.dataset.group = lastGroup = item.group
        children.push(heading)
      }
      children.push(tile(item))
    }
    grid.replaceChildren(...children)
    node.dataset.state = failure ? 'error' : busy ? 'loading' : filtered.length ? 'ready' : 'empty'
    detail.textContent = failure || (busy ? 'Loading GIFs…' : filtered.length ? '' : tab === 'gif' ? 'No matching GIFs' : tab === 'sticker' ? 'No matching server stickers' : 'No matching emoji')
  }
  const groups = [{ id: 'favorites', name: 'Favorites' }, ...tab === 'emoji' ? [{ id: 'frequent', name: 'Frequently Used' }] : []]
  if (tab === 'emoji') items = [...await unicodeEmojis()]
  if (!active()) return
  const currentGuild = channelById(channel)?.guildId, nitro = snapshot.user?.premiumType || 0
  if (tab !== 'gif') for (const guild of snapshot.guilds) {
    groups.push({ id: guild.id, name: guild.name })
    if (tab === 'emoji') for (const emoji of guild.emojis || []) {
      const roleAllowed = !emoji.roles?.length || guild.access?.member_roles?.some(id => emoji.roles.includes(id))
      const native = emoji.available && roleAllowed && (guild.id === currentGuild ? !emoji.animated || nitro > 0 : nitro > 0 && features.view.externalEmoji)
      items.push({ id: emoji.id, name: emoji.name, value: `<${emoji.animated ? 'a' : ''}:${emoji.name}:${emoji.id}>`, emoji: { id: emoji.id, name: emoji.name }, image: emojiUrl(emoji.id, emoji.animated, 64), enabled: !reaction || native, asLink: !native, group: guild.id, favoriteKind: 'emoji' })
    } else for (const sticker of guild.stickers || []) {
      const native = sticker.available && (guild.id === currentGuild || nitro > 1 && features.view.externalSticker)
      items.push({ id: sticker.id, name: sticker.name, aliases: sticker.tags, value: sticker.id, image: stickerImage(sticker), enabled: sticker.format_type !== 3 || !!native, asLink: !native, group: guild.id, favoriteKind: 'sticker' })
    }
  }
  for (const item of items) if (item.group && !groups.some(g => g.id === item.group)) groups.push({ id: item.group, name: item.group })
  const pinnedGroup = entry => entry.id === 'favorites' || entry.id === 'frequent'
  groups.sort((a, b) => Number(!pinnedGroup(a)) - Number(!pinnedGroup(b)) || Number(!guildById(a.id)) - Number(!guildById(b.id)))
  for (const entry of groups.filter(entry => pinnedGroup(entry) || items.some(item => item.group === entry.id))) {
    const option = element('option', '', entry.name), guild = guildById(entry.id)
    option.value = entry.id
    group.append(option)
    const control = button('', () => { group.value = entry.id; search.value = ''; group.dispatchEvent(new Event('change')) })
    control.title = entry.name
    control.setAttribute('aria-label', entry.name)
    control.dataset.group = entry.id
    if (guild) guildIcon(control, guild)
    else if (entry.id === 'frequent') control.append(icon('clock'))
    else control.textContent = entry.id === 'favorites' ? '★' : items.find(item => item.group === entry.id)?.value || '#'
    rail.append(control)
  }
  group.onchange = () => {
    limit = Math.max(limit, visible().findIndex(item => item.group === group.value) + 120)
    for (const control of rail.children) control.classList.toggle('selected', control.dataset.group === group.value)
    draw()
    grid.querySelector(`[data-group="${CSS.escape(group.value)}"]`)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
  }
  grid.onscroll = () => {
    if (total <= limit || grid.scrollTop + grid.clientHeight <= grid.scrollHeight - 600) return
    limit += 240
    redraw()
  }
  // Held arrows glide continuously, because restarting a smooth scroll on every key repeat crawls.
  let glide = 0
  const stopGlide = () => { cancelAnimationFrame(glide); glide = 0 }
  addEventListener('blur', stopGlide)
  node.addEventListener('close', () => { clearTimeout(timer); generation++; stopGlide(); removeEventListener('blur', stopGlide) }, { once: true })
  search.onkeyup = search.onblur = stopGlide
  search.onkeydown = event => {
    if (event.isComposing) return
    if (event.key === 'Enter') { event.preventDefault(); grid.querySelector('.expression:not(:disabled)')?.click() }
    const step = { ArrowDown: 1, ArrowUp: -1, PageDown: 4, PageUp: -4 }[event.key]
    if (!step) return
    event.preventDefault()
    if (!event.repeat) { grid.scrollBy({ top: step * ((grid.querySelector('.expression-tile')?.offsetHeight || 40) + 2), behavior: 'smooth' }); return }
    if (glide) return
    let last = performance.now()
    const frame = now => { grid.scrollTop += step * (now - last) * 1.2; last = now; glide = requestAnimationFrame(frame) }
    glide = requestAnimationFrame(frame)
  }
  search.oninput = () => {
    limit = 120
    failure = ''
    if (tab !== 'gif') { draw(); grid.scrollTop = 0; return }
    items = []
    busy = true
    draw()
    clearTimeout(timer)
    const ticket = ++generation, fresh = () => ticket === generation && active()
    timer = setTimeout(async () => {
      if (!fresh()) return
      try {
        const gifs = await cachedFeature('gifs', { query: search.value })
        if (!fresh()) return
        items = gifs.map(gif => ({ id: gif.id, name: gif.title || 'GIF', value: gif.url, image: gif.gifSrc || gif.src, enabled: true }))
      } catch (error) {
        if (!fresh()) return
        failure = error.message
      }
      busy = false
      draw()
    }, 250)
  }
  node.append(rail, tabs, search, group, grid, detail)
  rail.firstElementChild?.classList.add('selected')
  if (tab === 'gif') { group.hidden = true; search.dispatchEvent(new Event('input')) }
  else draw()
  search.focus()
}

const emojiFaces = ['😀', '😃', '😄', '😁', '😆', '😅', '😂', '🙂', '🙃', '😉', '😊', '😎', '🥳', '😍', '🤔', '😴']
$('#emoji-button').onpointerenter = () => { if (!features.picker?.open) $('#emoji-button .emoji-face').dataset.emoji = emojiFaces[Math.floor(Math.random() * emojiFaces.length)] }
$('#emoji-button').onclick = () => features.picker?.open && features.picker.dataset.tab === 'emoji' ? features.picker.close() : expressionPicker('emoji')
$('#sticker-button').onclick = () => expressionPicker('sticker')
$('#gif-button').onclick = () => expressionPicker('gif')
