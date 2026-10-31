// Message rendering: rich content, embeds, attachments, the image viewer, and edit and delete dialogs.
const revealedAttachments = new Set(), mediaRetries = new Map()

// Formatters are reused, as creating one costs about 50 µs and every message needs several.
const clockFormat = new Intl.DateTimeFormat([], { hour: '2-digit', minute: '2-digit', hour12: false })
const dayFormat = new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'short' })
const yearFormat = new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'short', year: 'numeric' })
const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' })
function clock(date) { return clockFormat.format(date) }
function messageTimestamp(date) {
  const today = new Date(), yesterday = new Date(today)
  yesterday.setDate(today.getDate() - 1)
  if (date.toDateString() === today.toDateString()) return 'Today at ' + clock(date)
  if (date.toDateString() === yesterday.toDateString()) return 'Yesterday at ' + clock(date)
  return (date.getFullYear() === today.getFullYear() ? dayFormat : yearFormat).format(date) + ' · ' + clock(date)
}
function relativeTime(date) {
  const seconds = Math.round((date.getTime() - Date.now()) / 1000), absolute = Math.abs(seconds)
  const [unit, size] = absolute < 60 ? ['second', 1] : absolute < 3600 ? ['minute', 60] : absolute < 86400 ? ['hour', 3600] : ['day', 86400]
  return relativeFormat.format(Math.round(seconds / size), unit)
}
function timestampLabel(date, format = 'f') {
  if (format === 'R') return relativeTime(date)
  const time = { hour: 'numeric', minute: '2-digit', ...format === 'T' ? { second: '2-digit' } : {} }
  if (format === 't' || format === 'T') return date.toLocaleTimeString([], time)
  const day = format === 'd' ? {} : { year: 'numeric', month: 'long', day: 'numeric', ...format === 'F' ? { weekday: 'long' } : {} }
  return format === 'd' || format === 'D' ? date.toLocaleDateString([], day) : date.toLocaleString([], { ...day, ...time })
}
setInterval(() => { for (const node of list.querySelectorAll('time[data-format="R"]')) node.textContent = relativeTime(new Date(node.dateTime)) }, 30000)

// Resolves the author's server nickname, avatar and coloured role.
function messageAuthor(message) {
  const guild = channelById(message.channelId)?.guildId, member = message.member || guildById(guild)?.members?.[message.authorId]
  let user = snapshot.users[message.authorId] || { id: message.authorId, name: message.author, avatar: message.avatar }
  if (member) user = memberUser(user, member, guild)
  return { guild, user, role: sortedRoles().find(role => member?.roles?.includes(role.id) && role.color) }
}
function renderMessage(message, index = rows.findIndex(row => row.id === message.id)) {
  const row = element('article', 'message')
  row.dataset.id = message.id
  const previous = rows[index - 1], timestamp = new Date(message.timestamp), last = previous && new Date(previous.timestamp)
  const unread = features.unreadAfter && BigInt(message.id) > BigInt(features.unreadAfter) && (!previous || BigInt(previous.id) <= BigInt(features.unreadAfter))
  const grouped = !unread && !message.reply && !message.reference && previous?.authorId === message.authorId && timestamp.toDateString() === last?.toDateString() && timestamp - last >= 0 && timestamp - last < 7 * 60 * 1000
  const { guild, user, role } = messageAuthor(message), own = message.authorId === snapshot.user?.id
  row.classList.toggle('grouped', grouped)
  row.classList.toggle('unread-boundary', !!unread)
  row.classList.toggle('mentioned', !!(message.mentionEveryone || message.mentions?.some(user => user.id === snapshot.user?.id) || message.mentionRoles?.some(role => guildById(guild)?.access?.member_roles?.includes(role))))
  row.setAttribute('aria-label', 'Message by ' + message.author)
  row.ondblclick = event => {
    if (event.target.closest('a,button,input,textarea,video,audio') || getSelection()?.toString()) return
    own ? editMessage(message) : startReply(message)
  }

  const avatar = element('button', 'avatar'), viewProfile = () => showProfile(user, !!guild, guild)
  avatar.append(userIcon(user))
  avatar.title = `View ${message.author}'s profile`
  avatar.onclick = viewProfile
  dragToken(avatar, 'user', message.authorId)
  const author = element('button', 'author', user.name)
  if (role) author.style.color = hex(role.color)
  author.onclick = viewProfile
  const time = element('time', 'message-time', messageTimestamp(timestamp))
  time.dateTime = message.timestamp
  time.title = timestamp.toLocaleString()
  const header = element('header'), body = element('div')
  header.append(author, time)
  if (message.editedTimestamp) header.append(element('small', 'muted', '(edited)'))
  if (message.reply || message.reference) {
    const reply = message.reply, reference = message.reference
    body.append(button(reply ? `↳ ${reply.author}: ${reply.content || (reply.attachment ? 'Attachment' : '')}` : '↳ Referenced message', () => reference && openMessage(reference.channel_id, reference.message_id), 'reply-preview'))
  }
  // HTML comes only from the server's Markdown renderer, which escapes raw HTML
  // and removes image tags and unsafe URL schemes.
  body.append(header, message.system ? element('p', 'system-message', message.system) : renderedContent(message.html, '', guild))
  for (const file of message.attachments || []) body.append(attachmentPreview(file, message))
  for (const [index, embed] of (message.embeds || []).entries()) body.append(renderEmbed(embed, message.rich?.embeds?.[index], message))
  for (const [index, forwarded] of (message.forwarded || []).entries()) {
    const node = element('section', 'forwarded'), reference = message.reference, rich = message.rich?.forwarded?.[index]
    const origin = channelById(reference?.channel_id), source = guildById(reference?.guild_id || origin?.guildId)
    const label = ['Forwarded message', source?.name, origin && '#' + origin.name, forwarded.timestamp && messageTimestamp(new Date(forwarded.timestamp))].filter(Boolean).join(' · ')
    node.append(element('small', 'muted', label), renderedContent(rich?.html, forwarded.content, source?.id))
    for (const attachment of forwarded.attachments || []) node.append(attachmentPreview(attachment, message))
    for (const [at, embed] of (forwarded.embeds || []).entries()) node.append(renderEmbed(embed, rich?.embeds?.[at], message))
    body.append(node)
  }
  for (const sticker of message.stickers || []) if (sticker.format_type !== 3) body.append(mediaImage(stickerImage(sticker), sticker.name, false))
  if (message.thread) body.append(button('Thread · ' + message.thread.name, () => selectChannel(message.thread.id), 'thread-link'))
  if (message.poll) body.append(renderPoll(message.poll))
  if (message.reactions) body.append(renderReactions(message))

  const actions = element('div', 'actions'), act = (name, label, run) => iconLabel(button('', run), name, label)
  actions.append(act('message-reply', 'Reply', () => startReply(message)))
  if (own) actions.append(act('edit', 'Edit', () => editMessage(message)), act('delete', 'Delete', () => deleteMessage(message)))
  actions.append(act('add-reaction', 'React', () => expressionPicker('emoji', message)))
  row.append(avatar, body, actions)
  if (grouped) {
    const stamp = element('time', 'group-time', clock(timestamp))
    stamp.title = time.title
    stamp.dateTime = message.timestamp
    row.append(stamp)
  }
  return row
}
function renderPoll(poll) {
  const node = element('section', 'poll'), results = poll.results, counts = results?.answer_counts || []
  const total = counts.reduce((sum, result) => sum + result.count, 0)
  node.append(element('strong', '', poll.question?.text || 'Poll'))
  for (const answer of poll.answers || []) {
    const media = answer.poll_media || {}, result = counts.find(count => count.id === answer.answer_id), line = element('p', result?.me_voted ? 'poll-selected' : '')
    if (media.emoji?.id) line.append(image(emojiUrl(media.emoji.id, false, 32), 'inline-emoji', media.emoji.name || 'Emoji'))
    else if (media.emoji?.name) line.append(media.emoji.name + ' ')
    line.append((media.text || '') + (results ? ` — ${result?.count || 0} (${total ? Math.round((result?.count || 0) / total * 100) : 0}%)` : '') + (result?.me_voted ? ' · Your selection' : ''))
    node.append(line)
  }
  if (!results) node.append(element('small', 'muted', 'Results unavailable'))
  const closed = results?.is_finalized || poll.expiry && new Date(poll.expiry).getTime() <= Date.now()
  node.append(element('small', 'muted', closed ? 'Poll closed' : poll.expiry ? 'Ends ' + new Date(poll.expiry).toLocaleString() : ''))
  if (results) node.append(element('small', 'muted', `${total} ${poll.allow_multiselect ? 'selections' : 'votes'}`))
  return node
}
function renderReactions(message) {
  const node = element('div', 'reactions')
  for (const reaction of message.reactions) {
    const emoji = reaction.emoji, label = emoji.name || 'Emoji', target = { channel: message.channelId, message: message.id, emoji }
    const control = button(`${label} ${reaction.count}`, async () => { await ui('reaction', { ...target, remove: reaction.me }); await refreshState(true) }, reaction.me ? 'reaction active' : 'reaction')
    if (emoji.id) control.replaceChildren(image(emojiUrl(emoji.id, false, 32), 'inline-emoji', label), ' ' + reaction.count)
    const previews = element('span', 'reaction-avatars')
    for (const user of (reaction.users || []).slice(0, 4)) previews.append(userIcon(user))
    if (reaction.count > 4 && reaction.users?.length) previews.append(element('small', '', '+' + (reaction.count - 4)))
    control.append(previews)
    control.oncontextmenu = event => { event.preventDefault(); peopleDialog('Reactions · ' + label, ui('reactors', target)) }
    control.title = 'Click to react · Right-click to view people'
    node.append(control)
  }
  return node
}
function renderedContent(html, fallback, guild) {
  const node = element('div', 'content')
  if (html != null) node.innerHTML = html
  else node.textContent = fallback || ''
  richContent(node, guild)
  return node
}
function linkNode(url, label) {
  const link = element('a', '', label)
  if (/^https?:\/\//u.test(url)) { link.href = url; link.target = '_blank'; link.rel = 'noopener noreferrer' }
  const target = /^https:\/\/(?:canary\.|ptb\.)?discord\.com\/channels\/([^/]+)\/(\d+)\/(\d+)/u.exec(url)
  if (target) link.onclick = event => { event.preventDefault(); openMessage(target[2], target[3]).catch(error => status(error.message)) }
  return link
}
function bareUrl(value) {
  const pairs = { ')': '(', ']': '[', '}': '{' }
  let end = value.length
  while (end) {
    const last = value[end - 1], part = value.slice(0, end)
    if (/[.,!?;:'"]/u.test(last) || pairs[last] && part.split(last).length > part.split(pairs[last]).length) end--
    else break
  }
  return value.slice(0, end)
}
// Replaces Discord tokens in text with mentions, emoji, timestamps and links, and wires spoilers.
function richContent(root, guild = channelById(channel)?.guildId) {
  root.dataset.richContent = ''
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT), nodes = []
  while (walker.nextNode()) if (!walker.currentNode.parentElement.closest('code,pre,a,button')) nodes.push(walker.currentNode)
  const token = /(<a?:[a-zA-Z0-9_]+:\d+>|<@!?\d+>|<@&\d+>|<#\d+>|<t:\d+(?::[tTdDfFR])?>|https?:\/\/[^\s<>]+|@everyone|@here)/gu
  const replacement = value => {
    if (/^<a?:/u.test(value)) {
      const [, animated, name, id] = /^<(a?):([^:]+):(\d+)>$/u.exec(value), node = image(emojiUrl(id, !!animated), 'inline-emoji', ':' + name + ':')
      node.title = node.alt
      return node
    }
    if (value.startsWith('<@&')) {
      const id = value.slice(3, -1)
      return button('@' + (roleById(id)?.name || id), () => roleDetails(id), 'mention')
    }
    if (value.startsWith('<@')) {
      const id = value.replace(/\D/gu, ''), user = contextualUser(id, guild), node = button('@' + (user?.name || id), () => showProfile(user || { id, name: id }, !!guild, guild), 'mention')
      dragToken(node, 'user', id)
      return node
    }
    if (value.startsWith('<#')) {
      const id = value.slice(2, -1), node = button('#' + (channelById(id)?.name || id), () => openChannel(id), 'mention')
      dragToken(node, 'channel', id)
      return node
    }
    if (value.startsWith('<t:')) {
      const [, seconds, format] = /^<t:(\d+)(?::([tTdDfFR]))?>$/u.exec(value), date = new Date(Number(seconds) * 1000)
      if (!Number.isFinite(date.getTime())) return value
      const node = element('time', 'timestamp', timestampLabel(date, format))
      node.dateTime = date.toISOString()
      node.dataset.format = format || 'f'
      node.title = date.toString()
      return node
    }
    if (value.startsWith('@')) return element('span', 'mention', value)
    const url = bareUrl(value), node = document.createDocumentFragment()
    node.append(linkNode(url, url), value.slice(url.length))
    return node
  }
  for (const text of nodes) {
    const fragment = document.createDocumentFragment()
    let copied = 0
    for (const match of text.data.matchAll(token)) {
      fragment.append(text.data.slice(copied, match.index), replacement(match[0]))
      copied = match.index + match[0].length
    }
    fragment.append(text.data.slice(copied))
    text.replaceWith(fragment)
  }
  for (const link of root.querySelectorAll('a')) {
    const safe = linkNode(link.href, '')
    safe.append(...link.childNodes)
    link.replaceWith(safe)
    // A fully concealed masked link must not remain keyboard-focusable outside
    // its text segments. The outer segment also hides the link's own styling.
    const parts = [...safe.querySelectorAll('.spoiler')], group = parts[0]?.dataset.spoiler
    if (!group || parts.some(part => part.dataset.spoiler !== group)) continue
    const text = document.createTreeWalker(safe, NodeFilter.SHOW_TEXT)
    let concealed = true
    while (text.nextNode()) if (text.currentNode.textContent.trim() && !text.currentNode.parentElement.closest('.spoiler')) concealed = false
    if (!concealed) continue
    const cover = element('span', 'spoiler')
    cover.dataset.spoiler = group
    cover.setAttribute('role', 'button')
    cover.tabIndex = 0
    safe.replaceWith(cover)
    cover.append(safe)
  }
  const reveal = (spoiler, toggle = false) => {
    const group = spoiler.dataset.spoiler, visible = toggle ? !spoiler.classList.contains('revealed') : true
    for (const part of spoiler.closest('[data-rich-content]').querySelectorAll('.spoiler')) if (part === spoiler || group && part.dataset.spoiler === group) part.classList.toggle('revealed', visible)
  }
  for (const spoiler of root.querySelectorAll('.spoiler')) {
    spoiler.onclick = event => {
      const part = event.currentTarget
      if (part.classList.contains('revealed')) return
      event.preventDefault()
      event.stopPropagation()
      reveal(part)
    }
    spoiler.onkeydown = event => {
      if (event.target !== event.currentTarget || !['Enter', ' '].includes(event.key)) return
      event.preventDefault()
      reveal(event.currentTarget, true)
    }
  }
  const plain = root.textContent.replace(/\s/gu, '')
  root.classList.toggle('emoji-only', !plain && !!root.querySelector('.inline-emoji') || !!plain && /^[\p{Extended_Pictographic}\p{Regional_Indicator}\p{Emoji_Modifier}\u200d\ufe0f\u20e3\d#*]+$/u.test(plain) && /[\p{Extended_Pictographic}\p{Regional_Indicator}\u20e3]/u.test(plain))
}

function mediaKind(file) {
  const mime = file.contentType || '', name = (file.filename || file.url || '').split(/[?#]/u)[0]
  for (const type of ['image', 'video', 'audio']) if (mime.startsWith(type + '/')) return type
  if (/\.(png|jpe?g|gif|webp|avif|bmp|svg)$/iu.test(name)) return 'image'
  if (/\.(mp4|webm|mov|m4v|mkv|ogv)$/iu.test(name)) return 'video'
  if (/\.(mp3|ogg|wav|flac|m4a|aac|opus)$/iu.test(name)) return 'audio'
  return ''
}
// Expired attachment links can be refreshed twice per message.
function bindMediaRetry(media, message) {
  if (!message) return
  media.onerror = () => {
    if (!media.isConnected || media.nextElementSibling?.classList.contains('media-retry')) return
    const account = snapshot.user?.id, current = staleGuard(), key = `${account}:${message.channelId}:${message.id}`
    const retry = element('button', 'media-retry', 'Preview unavailable. Refresh media')
    const exhausted = () => {
      retry.disabled = (mediaRetries.get(key) || 0) >= 2
      if (retry.disabled) retry.textContent = 'Preview unavailable. Try reopening this channel.'
    }
    retry.type = 'button'
    retry.onclick = async event => {
      event.stopPropagation()
      if (retry.disabled) return
      retry.disabled = true
      mediaRetries.set(key, (mediaRetries.get(key) || 0) + 1)
      try {
        const updated = await ui('refreshMessage', { channel: message.channelId, message: message.id }, account)
        if (!current() || updated?.id !== message.id) return
        const index = rows.findIndex(row => row.id === updated.id)
        if (index < 0) return
        rows = rows.with(index, updated)
        renderTimeline()
        if (!media.closest('#image-viewer')) return
        const entry = galleryImages().find(entry => entry.message.id === updated.id && entry.name === media.alt)
        if (!entry) return
        if (media._galleryEntry) Object.assign(media._galleryEntry, entry)
        media.src = entry.url
        retry.remove()
        bindMediaRetry(media, updated)
      } catch (error) { retry.textContent = error.message } finally { exhausted() }
    }
    exhausted()
    media.after(retry)
  }
}
function mediaImage(url, name, viewer = true, message = null) {
  const node = image(safeMedia(url), 'media-attachment', name || '')
  node.loading = 'lazy'
  if (viewer) node.onclick = () => imageViewer(message ? `${message.id}:${url}` : url, name)
  bindMediaRetry(node, message)
  return node
}
function attachmentPreview(file, message = null) {
  const tile = element('div', 'attachment-media')
  tile._attachmentKey = JSON.stringify(file)
  const paint = () => {
    tile.replaceChildren()
    const type = mediaKind(file), url = safeMedia(file.proxyUrl || file.url)
    const media = !url ? null : type === 'image' ? element('img', 'media-attachment') : type === 'video' || type === 'audio' ? element(type, 'media-attachment') : null
    if (media) {
      media.src = url
      if (type === 'image') {
        if (file.width > 0 && file.height > 0) {
          const scale = Math.min(1, 400 / file.width, 300 / file.height)
          media.width = Math.round(file.width * scale)
          media.height = Math.round(file.height * scale)
        }
        media.alt = file.filename
        media.loading = 'lazy'
        media.onclick = () => imageViewer(message ? `${message.id}:${file.id || file.url}` : url, file.filename)
      } else {
        media.controls = true
        media.preload = 'metadata'
      }
      bindMediaRetry(media, message)
      tile.append(media)
    }
    const link = element('a', media ? 'attachment attachment-download' : 'attachment')
    if (/^https?:\/\//u.test(file.url)) { link.href = file.url; link.target = '_blank'; link.rel = 'noopener noreferrer' }
    if (media) iconLabel(link, 'arrow-down', 'Download ' + file.filename)
    else link.append(element('span', '', file.filename), element('span', '', '↓'))
    tile.append(link)
  }
  if (file.filename.startsWith('SPOILER_') && !revealedAttachments.has(file.url)) {
    const reveal = button('Reveal spoiler', () => { revealedAttachments.add(file.url); paint() }, 'attachment-spoiler')
    reveal.setAttribute('aria-label', 'Reveal spoiler attachment')
    tile.append(reveal)
  } else paint()
  return tile
}
function renderEmbed(embed, rich = {}, message = null) {
  const node = element('section', 'embed'), guild = channelById(message?.channelId || channel)?.guildId
  const smallImage = (item, parent) => { const url = item?.proxy_icon_url || item?.icon_url; if (safeMedia(url)) parent.append(image(url)) }
  if (embed.color != null) node.style.borderLeftColor = hex(embed.color)
  if (embed.provider?.name) node.append(embed.provider.url ? linkNode(embed.provider.url, embed.provider.name) : element('small', 'muted', embed.provider.name))
  if (embed.author?.name) {
    const author = element('div', 'embed-author')
    smallImage(embed.author, author)
    author.append(embed.author.url ? linkNode(embed.author.url, embed.author.name) : element('strong', '', embed.author.name))
    node.append(author)
  }
  if (embed.title) node.append(embed.url ? linkNode(embed.url, embed.title) : element('h4', '', embed.title))
  if (embed.description) node.append(renderedContent(rich.description, embed.description, guild))
  for (const [index, field] of (embed.fields || []).entries()) {
    const row = element('div', field.inline ? 'embed-field inline' : 'embed-field')
    row.append(element('strong', '', field.name), renderedContent(rich.fields?.[index], field.value, guild))
    node.append(row)
  }
  const gif = embed.type === 'gifv'
  if (!gif && embed.thumbnail && embed.thumbnail.url !== embed.image?.url) node.append(mediaImage(embed.thumbnail.proxy_url || embed.thumbnail.url, '', true, message))
  if (!gif && embed.image) node.append(mediaImage(embed.image.proxy_url || embed.image.url, embed.title, true, message))
  const video = embed.video?.proxy_url || embed.video?.url
  if (safeMedia(video) && (gif || /\.(mp4|webm)(\?|$)/iu.test(video))) {
    const media = element('video', 'media-attachment')
    Object.assign(media, { src: video, controls: !gif, autoplay: false, muted: gif, loop: gif, playsInline: true, preload: 'metadata' })
    if (gif) media.dataset.animated = 'true'
    bindMediaRetry(media, message)
    node.append(media)
  }
  if (embed.footer?.text || embed.timestamp) {
    const footer = element('small', 'muted embed-footer')
    smallImage(embed.footer, footer)
    footer.append([embed.footer?.text, embed.timestamp && messageTimestamp(new Date(embed.timestamp))].filter(Boolean).join(' · '))
    node.append(footer)
  }
  return node
}

function galleryImages() {
  const images = [], seen = new Set()
  const add = entry => { if (!seen.has(entry.id)) { seen.add(entry.id); images.push(entry) } }
  for (const message of rows) for (const source of [message, ...message.forwarded || []]) {
    for (const file of source.attachments || []) if (mediaKind(file) === 'image' && (!file.filename.startsWith('SPOILER_') || revealedAttachments.has(file.url))) add({ id: `${message.id}:${file.id || file.url}`, url: file.proxyUrl || file.url, original: file.url, name: file.filename, message })
    for (const embed of source.embeds || []) for (const item of [embed.image, embed.thumbnail]) if (item) add({ id: `${message.id}:${item.url}`, url: item.proxy_url || item.url, original: item.url, proxyId: `${message.id}:${item.proxy_url || item.url}`, name: embed.title || 'Image', message })
  }
  return images.filter(entry => safeMedia(entry.url))
}
function imageViewer(url, name) {
  const matches = entry => entry.url === url || entry.original === url || entry.id === url || entry.proxyId === url
  let images = galleryImages()
  const owner = images.find(matches)?.message?.id
  if (owner) images = images.filter(entry => entry.message.id === owner)
  let index = images.findIndex(matches)
  if (index < 0) { images.push({ url, original: url, name }); index = images.length - 1 }
  const node = modal(name || 'Image', 'image-viewer'), picture = element('img'), controls = element('div', 'viewer-controls')
  node.id = 'image-viewer'
  const paint = () => {
    const entry = images[index]
    picture._galleryEntry = entry
    if (picture.nextElementSibling?.classList.contains('media-retry')) picture.nextElementSibling.remove()
    picture.src = entry.url
    picture.alt = entry.name || ''
    bindMediaRetry(picture, entry.message)
    node.querySelector('h2').textContent = `${entry.name || 'Image'} · ${index + 1}/${images.length}`
  }
  const step = delta => { index = (index + delta + images.length) % images.length; paint() }
  picture.onclick = () => picture.classList.toggle('zoomed')
  controls.append(
    button('Previous', () => step(-1)),
    button('Next', () => step(1)),
    button('Open original', () => { if (safeMedia(images[index].original)) open(images[index].original, '_blank', 'noopener,noreferrer') }),
    button('Fullscreen', () => node.requestFullscreen()),
  )
  node.onkeydown = event => {
    if (event.key === 'ArrowLeft') step(-1)
    if (event.key === 'ArrowRight') step(1)
  }
  node.append(picture, controls)
  paint()
}

function deleteMessage(message) {
  confirmDialog('Delete message?', message.content || 'Attachment', 'Delete', () => ui('delete', { channel: message.channelId, message: message.id }))
}
function editMessage(message) {
  const node = modal('Edit message', 'edit-dialog'), text = element('textarea'), attachments = element('div'), input = element('input'), error = element('p', 'error')
  const retained = new Set((message.attachments || []).map(file => file.id)), account = snapshot.user.id
  text.value = message.content
  text.maxLength = 2000
  text.rows = 5
  text.setAttribute('aria-label', 'Edit message content')
  input.type = 'file'
  input.multiple = true
  for (const attachment of message.attachments || []) {
    const row = element('div', 'file', attachment.filename)
    row.append(button('Remove', () => { retained.delete(attachment.id); row.remove() }))
    attachments.append(row)
  }
  node.append(text, attachments, input, error, button('Save', async () => {
    error.textContent = ''
    try {
      await withUploads(input.files, account, uploaded => ui('edit', { channel: message.channelId, message: message.id, content: text.value, retained: [...retained], files: uploaded }, account))
      node.close()
      await refreshState(true)
    } catch (failure) { error.textContent = failure.message }
  }))
  text.focus()
}
