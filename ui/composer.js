// Keep wire tokens in the composer value while rendering mentions as atomic chips.
function createComposer(node) {
  const tokenPattern = /<(@!?|@&|#)(\d+)>/gu
  let disabled = false, readOnly = false, composing = false
  let selectionStart = 0, selectionEnd = 0
  let history = [{ value: '', start: 0, end: 0 }], cursor = 0, lastKind = '', lastEdit = 0
  function state() { return { value: text(), start: offset(false), end: offset(true) } }
  function resetHistory() { history = [state()]; cursor = 0; lastKind = '' }
  function beforeEdit() { history[cursor] = state() }
  function committed(kind = '') {
    const current = state(), previous = history[cursor]
    if (current.value === previous.value) return
    const now = performance.now(), merge = /^(insertText|deleteContentBackward|deleteContentForward)$/u.test(kind) && kind === lastKind && now - lastEdit < 1000 && cursor > 0
    history.splice(cursor + 1)
    if (merge) history[cursor] = current
    else {
      history.push(current)
      cursor++
      if (history.length > 100) { history.shift(); cursor-- }
    }
    lastKind = kind
    lastEdit = now
  }
  function undo(redo = false) {
    const target = cursor + (redo ? 1 : -1)
    if (target < 0 || target >= history.length) return
    cursor = target
    const current = history[cursor]
    node.replaceChildren(fragment(current.value))
    select(current.start, current.end)
    lastKind = ''
    node.dispatchEvent(new Event('input', { bubbles: true }))
  }
  // A trailing newline only renders a new line when another newline follows it,
  // so the value drops one trailing newline and fragment() adds it back.
  function raw(root) {
    if (root.nodeType === Node.TEXT_NODE) return root.data
    if (root.dataset?.token) return root.dataset.token
    let value = ''
    for (const child of root.childNodes) value += raw(child)
    return value
  }
  function text() { return raw(node).replace(/\n$/u, '') }
  function chip(token, kind, id) {
    const role = kind === '@&' && roleById(id)
    const user = snapshot && kind !== '#' && contextualUser(id, channelById(channel)?.guildId)
    const item = snapshot && kind === '#' && channelById(id)
    if (!role && !user && !item) return document.createTextNode(token)
    const span = element('span', 'draft-mention')
    span.dataset.token = token
    span.contentEditable = 'false'
    if (user) {
      const avatar = userIcon(user)
      avatar.setAttribute('aria-hidden', 'true')
      span.append(avatar)
    }
    span.append((kind === '#' ? '#' : '@') + (role?.name || user?.name || item?.name))
    if (role?.color) span.style.setProperty('--role-color', hex(role.color))
    return span
  }
  function fragment(value) {
    const result = document.createDocumentFragment()
    let copied = 0
    for (const match of value.matchAll(tokenPattern)) {
      result.append(value.slice(copied, match.index), chip(match[0], match[1], match[2]))
      copied = match.index + match[0].length
    }
    result.append(value.slice(copied) + (value.endsWith('\n') ? '\n' : ''))
    return result
  }
  function offset(end) {
    const selection = getSelection(), length = text().length
    if (!selection.rangeCount || !node.contains(selection.anchorNode) || !node.contains(selection.focusNode)) return Math.min(end ? selectionEnd : selectionStart, length)
    const selected = selection.getRangeAt(0), range = document.createRange()
    range.selectNodeContents(node)
    range.setEnd(end ? selected.endContainer : selected.startContainer, end ? selected.endOffset : selected.startOffset)
    return Math.min(raw(range.cloneContents()).length, length)
  }
  function point(at) {
    let left = Math.max(0, at)
    for (const [index, child] of [...node.childNodes].entries()) {
      const size = child.nodeType === Node.TEXT_NODE ? child.length : child.dataset?.token?.length || 0
      if (child.nodeType === Node.TEXT_NODE ? left <= size : left < size) return child.nodeType === Node.TEXT_NODE ? [child, left] : [node, index]
      left -= size
    }
    return [node, node.childNodes.length]
  }
  function select(start, end = start) {
    const range = document.createRange()
    range.setStart(...point(start))
    range.setEnd(...point(end))
    const selection = getSelection()
    selection.removeAllRanges()
    selection.addRange(range)
    selectionStart = offset(false)
    selectionEnd = offset(true)
  }
  function rememberSelection() {
    const selection = getSelection()
    if (selection.rangeCount && node.contains(selection.anchorNode) && node.contains(selection.focusNode)) {
      selectionStart = offset(false)
      selectionEnd = offset(true)
    }
  }
  function tooLong(value) {
    if ([...value].length <= 2000) return false
    status('Messages must be 2,000 characters or fewer.')
    return true
  }
  function replace(value, start, end) {
    const next = text().slice(0, start) + value + text().slice(end)
    if (tooLong(next)) return
    beforeEdit()
    node.replaceChildren(fragment(next))
    select(start + value.length)
    committed()
  }
  function editable() {
    node.contentEditable = disabled || readOnly ? 'false' : 'plaintext-only'
    node.setAttribute('aria-disabled', String(disabled))
    node.setAttribute('aria-readonly', String(readOnly))
  }
  document.addEventListener('selectionchange', rememberSelection)
  node.addEventListener('blur', rememberSelection)
  Object.defineProperties(node, {
    value: {
      get: () => text(),
      set: value => {
        node.replaceChildren(fragment(String(value)))
        selectionStart = selectionEnd = String(value).length
        if (document.activeElement === node) select(String(value).length)
        resetHistory()
      },
    },
    selectionStart: { get: () => offset(false) },
    selectionEnd: { get: () => offset(true) },
    disabled: { get: () => disabled, set: value => { disabled = !!value; editable() } },
    readOnly: { get: () => readOnly, set: value => { readOnly = !!value; editable() } },
    placeholder: { get: () => node.dataset.placeholder || '', set: value => { node.dataset.placeholder = value } },
  })
  node.setRangeText = (value, start = offset(false), end = offset(true)) => replace(value, start, end)
  // Native plaintext pasting turns a trailing newline into a block, so pasting goes through replace().
  node.addEventListener('paste', event => {
    if ([...event.clipboardData.items].some(item => item.kind === 'file')) return
    event.preventDefault()
    if (disabled || readOnly) return
    replace(event.clipboardData.getData('text/plain'), offset(false), offset(true))
    node.dispatchEvent(new Event('input', { bubbles: true }))
  })
  node.addEventListener('input', event => {
    if (event.isComposing || composing) return
    if (tooLong(text())) {
      const previous = history[cursor]
      node.replaceChildren(fragment(previous.value))
      select(previous.start, previous.end)
      return
    }
    if (!text() && node.childNodes.length) node.replaceChildren()
    // Typed or pasted tokens become chips.
    if ([...node.childNodes].some(child => child.nodeType === Node.TEXT_NODE && /<(@!?|@&|#)\d+>/u.test(child.data))) {
      const start = offset(false), end = offset(true)
      node.replaceChildren(fragment(text()))
      select(start, end)
    }
    committed(event.inputType)
  })
  node.addEventListener('beforeinput', event => {
    if (disabled || readOnly) { event.preventDefault(); return }
    if (event.inputType === 'historyUndo' || event.inputType === 'historyRedo') {
      event.preventDefault()
      undo(event.inputType === 'historyRedo')
      return
    }
    if (composing) return
    if (event.inputType.startsWith('insert') && !event.inputType.includes('Composition') && tooLong(text().slice(0, offset(false)) + (event.data || '') + text().slice(offset(true)))) {
      event.preventDefault()
      return
    }
    beforeEdit()
  })
  node.addEventListener('keydown', event => {
    if ((event.ctrlKey || event.metaKey) && !event.altKey && ['z', 'y'].includes(event.key.toLowerCase())) {
      event.preventDefault()
      event.stopImmediatePropagation()
      if (!disabled && !readOnly) undo(event.shiftKey || event.key.toLowerCase() === 'y')
    }
  })
  node.addEventListener('compositionstart', () => { beforeEdit(); composing = true })
  node.addEventListener('compositionend', () => { composing = false; node.dispatchEvent(new Event('input', { bubbles: true })) })
  editable()
  return node
}

const draft = createComposer($('#draft'))
let sending = false
let mentionIndex = 0, mentionRows = [], mentionStart = -1, mentionTimer, mentionQuery = null, mentionContext
let mentionDismissed = false

function canSend() {
  return !!snapshot?.user && clientState?.ready && snapshot.sendable.has(channel) && Date.now() >= (features.slowUntil.get(channel) || 0)
}
function setComposer(disabled) {
  draft.disabled = disabled
  for (const id of ['#send', '#attach', '#gif-button', '#sticker-button', '#emoji-button']) $(id).disabled = sending || disabled
}
function renderComposerState() {
  const slow = Math.max(0, Math.ceil(((features.slowUntil.get(channel) || 0) - Date.now()) / 1000))
  $('#composer-hint').textContent = channel && !snapshot.sendable.has(channel) ? 'You cannot send messages in this channel.'
    : slow ? `Slow mode · ${slow}s remaining` : features.view.slowmode ? `Slow mode · ${features.view.slowmode}s` : ''
  setComposer(!canSend())
}
function resizeDraft() {
  draft.style.height = 'auto'
  draft.style.height = Math.min(160, draft.scrollHeight) + 'px'
  if (following) bottom()
}
function insertDraft(value) {
  if (draft.disabled || draft.readOnly) return
  draft.setRangeText(value, draft.selectionStart, draft.selectionEnd, 'end')
  draft.dispatchEvent(new Event('input'))
  draft.focus()
}
function renderReply() {
  const node = $('#draft-context')
  node.replaceChildren()
  node.hidden = !features.reply && !features.sticker
  if (features.reply) node.append(element('span', '', 'Replying to ' + features.reply.author), button('Cancel reply', () => { features.reply = null; renderReply() }))
  if (features.sticker) node.append(element('span', '', 'Sticker: ' + features.sticker.name), button('Remove sticker', () => { features.sticker = null; renderReply() }))
}
function startReply(message) {
  features.reply = message
  renderReply()
  draft.focus()
}
function chooseSticker(id, name) {
  if (draft.disabled || draft.readOnly) return
  features.sticker = { id, name }
  files = []
  renderFiles()
  renderReply()
}
// Stickers from other servers are sent as links when they cannot be sent natively.
async function prepareSticker(id, name, active = () => true) {
  const current = staleGuard()
  const prepared = await ui('prepareSticker', { channel, sticker: id })
  if (!current() || !active()) return
  if (prepared.sticker) chooseSticker(prepared.sticker, name)
  else if (prepared.link) insertDraft(prepared.link)
}
function renderFiles() {
  $('#file-list').replaceChildren(...files.map((file, index) => {
    const node = element('span', 'file', file.name), remove = element('button', '', '×')
    remove.title = 'Remove ' + file.name
    remove.onclick = () => { files.splice(index, 1); renderFiles(); draft.focus() }
    node.append(remove)
    return node
  }))
}
function addFiles(picked) {
  if (sending || !channel) return
  if (picked.length) { features.sticker = null; renderReply() }
  for (const file of picked) {
    if (file.size > fileLimit) { status('Each file must be 10 MiB or smaller.'); continue }
    if (files.length === 10) { status('You can attach up to 10 files.'); break }
    files.push(file)
  }
  renderFiles()
  draft.focus()
}
function renderTyping() {
  const ids = (clientState?.typing?.[channel] || []).filter(id => id !== snapshot.user?.id)
  const guild = guildById(selectedGuild)
  const names = ids.map(id => guild?.members?.[id]?.nick || snapshot.users[id]?.name || 'Someone')
  const label = names.length > 3 ? 'Several people are typing…' : names.length ? names.join(', ') + (names.length === 1 ? ' is typing…' : ' are typing…') : ''
  const node = $('#typing')
  if (node.dataset.label === label) return
  node.dataset.label = label
  node.replaceChildren()
  if (!label) return
  const dots = element('span', 'typing-dots')
  dots.setAttribute('aria-hidden', 'true')
  dots.append(element('i'), element('i'), element('i'))
  node.append(dots, label)
}

function channelMentionUsers() {
  const session = featureSession()
  if (mentionContext?.snapshot === snapshot && mentionContext.rows === rows && mentionContext.channel === channel && mentionContext.session === session) return mentionContext.users
  const item = channelById(channel), guild = guildById(item?.guildId)
  const ids = new Set([snapshot.user?.id, ...item?.recipientIds || [], ...Object.keys(guild?.members || {})]), fallback = new Map()
  for (const message of rows) {
    ids.add(message.authorId)
    fallback.set(message.authorId, { id: message.authorId, name: message.author, avatar: message.avatar })
    for (const user of message.mentions || []) { ids.add(user.id); fallback.set(user.id, user) }
  }
  const candidates = new Map()
  for (const id of ids) {
    const user = contextualUser(id, guild?.id) || fallback.get(id)
    if (user) candidates.set(id, { ...user, token: '<@' + id + '>' })
  }
  mentionContext = { snapshot, rows, channel, session, users: candidates }
  return candidates
}
function updateMentions(remote = true) {
  if (remote) mentionDismissed = false
  const prefix = draft.value.slice(0, draft.selectionStart), match = draft.disabled || draft.readOnly || mentionDismissed ? null : /(?:^|\s)([@#])([^\s@#]*)$/u.exec(prefix)
  const previous = mentionRows[mentionIndex]?.id, needle = match?.[2].toLowerCase() || ''
  mentionStart = match ? prefix.length - match[2].length - 1 : -1
  const candidates = !match ? []
    : match[1] === '#' ? snapshot.channels.filter(c => c.guildId === selectedGuild && c.kind !== 4).map(c => ({ id: c.id, name: '#' + c.name, token: '<#' + c.id + '>' }))
      : [...channelMentionUsers().values(), ...(features.view.roles || []).map(r => ({ id: r.id, name: '@' + r.name, token: '<@&' + r.id + '>' }))]
  mentionRows = candidates.filter(item => item.name.toLowerCase().includes(needle)).slice(0, 8)
  mentionIndex = Math.max(0, mentionRows.findIndex(item => item.id === previous))
  drawMentions()
  const key = match?.[1] === '@' && needle ? JSON.stringify([featureSession().session, channel, request, needle]) : null
  if (key !== mentionQuery) { clearTimeout(mentionTimer); mentionTimer = null; mentionQuery = key }
  if (!key) { mentionTimer = null; return }
  if (!remote || mentionTimer != null) return
  const value = draft.value, caret = draft.selectionStart, current = staleGuard()
  mentionTimer = setTimeout(async () => {
    if (!current() || key !== mentionQuery) return
    try {
      const found = await ui('searchMembers', { channel, query: needle })
      if (draft.value !== value || draft.selectionStart !== caret || !current() || key !== mentionQuery) return
      const candidates = channelMentionUsers()
      for (const user of found) candidates.set(user.id, { ...user, token: '<@' + user.id + '>' })
      updateMentions(false)
    } catch (error) { if (key === mentionQuery && current()) status(error.message) }
  }, 250)
}
function drawMentions() {
  const popup = $('#suggestions')
  const signature = JSON.stringify([mentionIndex, mentionRows.map(({ id, name, token }) => [id, name, token])])
  if (popup.dataset.signature === signature) return
  popup.dataset.signature = signature
  popup.hidden = !mentionRows.length
  draft.setAttribute('aria-expanded', String(!popup.hidden))
  draft.setAttribute('aria-controls', 'suggestions')
  if (popup.hidden) draft.removeAttribute('aria-activedescendant')
  else draft.setAttribute('aria-activedescendant', 'mention-' + mentionIndex)
  popup.replaceChildren(...mentionRows.map((user, index) => {
    const option = element('button', '', user.name)
    option.id = 'mention-' + index
    option.setAttribute('role', 'option')
    option.setAttribute('aria-selected', String(index === mentionIndex))
    option.onmousedown = event => event.preventDefault()
    option.onclick = () => insertMention(user)
    return option
  }))
}
function insertMention(user) {
  if (draft.disabled || draft.readOnly) return
  draft.setRangeText(user.token + ' ', mentionStart, draft.selectionStart, 'end')
  mentionRows = []
  drawMentions()
  draft.focus()
  resizeDraft()
}

$('#composer').onsubmit = async event => {
  event.preventDefault()
  if ((!draft.value.trim() && !files.length && !features.sticker) || sending || !canSend()) return
  const id = channel, account = clientState?.account || snapshot?.user?.id, content = draft.value, pending = files.slice()
  const reply = features.reply?.id || null, sticker = features.sticker?.id || null
  sending = true
  setComposer(false)
  draft.readOnly = true
  try {
    await withUploads(pending, account, uploaded => {
      const nonce = String(BigInt(Date.now()) * 1048576n + BigInt(crypto.getRandomValues(new Uint32Array(1))[0] % 1048576))
      return ui('send', { channel: id, content, files: uploaded, nonce, reply, sticker }, account)
    }, file => status('Uploading ' + file.name))
    if (account !== clientState?.account) return
    if (channel === id) {
      draft.value = ''
      features.reply = features.sticker = null
      features.extras.delete(id)
      renderReply()
      if (features.view.slowmode) features.slowUntil.set(id, Date.now() + features.view.slowmode * 1000)
      files = []
      renderFiles()
      resizeDraft()
      const current = staleGuard()
      if (features.context) {
        if (await loadHistory('latest') && current()) bottom()
      } else {
        const previousRows = rows, messages = await api('messages/' + encodeURIComponent(id))
        if (current() && rows === previousRows && !features.loadingHistory) {
          rows = messages
          renderTimeline({ follow: true })
          bottom()
        }
      }
    } else {
      drafts.delete(id)
      features.extras.delete(id)
    }
    status('')
  } catch (error) {
    status(error.message)
  } finally {
    sending = false
    setComposer(!canSend())
    draft.readOnly = false
    draft.focus()
  }
}
draft.addEventListener('input', () => { resizeDraft(); updateMentions() })
draft.addEventListener('click', () => updateMentions())
draft.addEventListener('keyup', event => { if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) updateMentions() })
draft.addEventListener('keydown', event => {
  if (event.isComposing) return
  if (mentionRows.length && ['Tab', 'ArrowDown', 'ArrowUp', 'Enter', 'Escape'].includes(event.key)) {
    event.preventDefault()
    if (event.key === 'Enter') insertMention(mentionRows[mentionIndex])
    else if (event.key === 'Escape') { mentionDismissed = true; updateMentions(false) }
    else {
      mentionIndex = (mentionIndex + (event.key === 'ArrowUp' || event.shiftKey ? -1 : 1) + mentionRows.length) % mentionRows.length
      drawMentions()
    }
  } else if (event.key === 'Enter' && !event.shiftKey) {
    event.preventDefault()
    $('#composer').requestSubmit()
  } else if (event.key === 'ArrowUp' && !draft.value && !dialogOpen()) {
    const message = rows.findLast(message => message.authorId === snapshot.user?.id)
    if (message) { event.preventDefault(); editMessage(message) }
  }
})
draft.addEventListener('paste', event => {
  const pasted = [...event.clipboardData.items].filter(item => item.kind === 'file').map(item => item.getAsFile()).filter(Boolean)
  if (pasted.length) { event.preventDefault(); addFiles(pasted) }
})
$('#attach').onclick = () => $('#file-input').click()
$('#file-input').onchange = event => { addFiles(event.target.files); event.target.value = '' }
app.ondragover = event => {
  const types = event.dataTransfer.types
  if (types.includes('Files')) { event.preventDefault(); app.classList.add('drop-target') }
  else if (types.includes('application/x-suncord') && event.target.closest('#compose')) event.preventDefault()
}
app.ondragleave = event => { if (!app.contains(event.relatedTarget)) app.classList.remove('drop-target') }
app.ondrop = async event => {
  event.preventDefault()
  app.classList.remove('drop-target')
  const token = event.dataTransfer.getData('application/x-suncord')
  if (!token) return addFiles(event.dataTransfer.files)
  if (!event.target.closest('#compose')) return
  try {
    const item = JSON.parse(token)
    if (item.kind === 'sticker') await prepareSticker(item.id, item.name)
    else if (item.kind === 'user') insertDraft(`<@${item.id}>`)
    else if (item.kind === 'channel') insertDraft(`<#${item.id}>`)
    else if (item.kind === 'guild') {
      const target = snapshot.channels.find(c => c.guildId === item.id && c.kind === 0)
      if (target) {
        const current = staleGuard(), invite = await ui('invite', { channel: target.id })
        if (current()) insertDraft(invite)
      }
    } else insertDraft(item.value)
  } catch (error) { status(error.message) }
}
