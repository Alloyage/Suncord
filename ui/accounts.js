// Saved accounts, sign-in and settings.
const accountMenu = $('#account-menu')
let preferences = Promise.resolve()

function renderAccounts() {
  const link = clientState.loginUrl, list = $('#account-list')
  $('#notice').textContent = clientState.status
  $('#account-status').textContent = clientState.status || ''
  $('#account-status').hidden = clientState.connected || !clientState.status
  $('#login-link').hidden = $('#cancel-login').hidden = !link
  if (link) $('#login-link').href = link
  $('#disconnect-account').hidden = !clientState.connected
  $('#resume-network').hidden = !clientState.account || clientState.connected
  const signature = JSON.stringify([clientState.accounts, clientState.account, clientState.connected])
  if (list.dataset.signature === signature) return
  list.dataset.signature = signature
  list.replaceChildren(...clientState.accounts.map(user => {
    const row = element('div', 'account-row'), choice = element('button', 'account-choice person'), label = element('span', 'account-label')
    const active = user.id === clientState.account
    label.append(element('strong', '', user.name), element('small', '', user.username ? '@' + user.username : user.id))
    choice.append(userIcon(user), label)
    choice.classList.toggle('active', active)
    choice.setAttribute('aria-current', String(active))
    choice.onclick = () => { if (active && clientState.connected) accountMenu.hidePopover(); else accountAction('connect', { account: user.id }) }
    const remove = iconLabel(element('button', 'remove-account icon-button'), 'close', 'Remove ' + user.name)
    remove.onclick = () => { accountMenu.hidePopover(); removeAccount(user) }
    row.append(choice, remove)
    return row
  }))
}
async function accountAction(name, values = {}) {
  try {
    await action(name, values)
    await refreshState(true)
    if (clientState.connected) accountMenu.hidePopover()
  } catch (error) {
    $('#account-status').textContent = error.message
    $('#account-status').hidden = false
    status(error.message)
  }
}
function removeAccount(user) {
  const account = snapshot?.user?.id || ''
  confirmDialog('Remove saved account?', `Remove ${user.name}'s saved sign-in from this device?`, 'Remove account', async () => {
    await action('forgetAccount', { account: user.id }, account)
    if (snapshot.user?.id === user.id) resetAccount()
  })
}
// Clears everything tied to the signed-in account before another one is shown.
function resetAccount() {
  closePlayback()
  ignoredRings.clear()
  leaveChannel(null)
  drafts.clear()
  features.extras.clear()
  features.slowUntil.clear()
  features.notices = null
  features.role = null
  revealedAttachments.clear()
  showSidePanel(null)
  accountMenu.hidePopover()
  for (const node of document.querySelectorAll('dialog')) node.close()
  selectedGuild = undefined
  structure = null
}

accountMenu.addEventListener('toggle', event => {
  $('#accounts-button').setAttribute('aria-expanded', String(event.newState === 'open'))
  if (event.newState === 'open') accountMenu.querySelector('button:not(:disabled)')?.focus()
})
accountMenu.onkeydown = event => {
  event.stopPropagation()
  if (!['ArrowUp', 'ArrowDown', 'Home', 'End', 'Tab'].includes(event.key)) return
  const controls = [...accountMenu.querySelectorAll('button:not(:disabled),a[href]')].filter(node => !node.hidden)
  if (!controls.length) return
  event.preventDefault()
  const index = controls.indexOf(document.activeElement), back = event.key === 'ArrowUp' || event.key === 'Tab' && event.shiftKey
  controls[event.key === 'Home' ? 0 : event.key === 'End' ? controls.length - 1 : (index + (back ? -1 : 1) + controls.length) % controls.length].focus()
}
$('#accounts-button').onclick = () => accountMenu.togglePopover()
$('#login').onclick = () => accountAction('login')
$('#cancel-login').onclick = () => accountAction('cancelLogin')
$('#disconnect-account').onclick = () => accountAction('disconnect')
$('#resume-network').onclick = () => accountAction('resume', { account: clientState.account })

function loadPreferences() {
  preferences = ui('preferences').then(value => { features.prefs = value; applyPreferences() }, error => status(error.message))
}
function applyPreferences() {
  panelWidths()
  document.documentElement.style.setProperty('--avatar-roundness', features.prefs.avatarRoundness / 2 + '%')
}
function preferencesPanel() {
  const node = modal('Settings', 'settings-dialog'), form = element('form'), error = element('p', 'error')
  const save = async () => {
    try {
      const fields = form.elements
      features.prefs = await ui('preferences', { value: { ...features.prefs, notifications: fields.notifications.checked, showInaccessibleChannels: fields.showInaccessibleChannels.checked, avatarRoundness: Number(fields.avatarRoundness.value) } })
      applyPreferences()
      structure = null
      renderSnapshot()
      error.textContent = ''
    } catch (failure) { error.textContent = failure.message }
  }
  const toggle = (key, label) => {
    const row = element('label', 'check-setting'), input = element('input')
    input.type = 'checkbox'
    input.name = key
    input.checked = features.prefs[key]
    input.onchange = save
    row.append(input, label)
    form.append(row)
  }
  form.append(element('h3', 'settings-section', 'NOTIFICATIONS'))
  toggle('notifications', 'DM and mention notifications')
  if ('Notification' in window && Notification.permission !== 'granted') form.append(button('Allow notifications', () => Notification.requestPermission()))
  form.append(element('h3', 'settings-section', 'APPEARANCE'))
  const row = element('label', 'range-setting'), range = element('input'), value = element('output', '', features.prefs.avatarRoundness + '%')
  Object.assign(range, { type: 'range', min: '0', max: '100', value: features.prefs.avatarRoundness, name: 'avatarRoundness', onchange: save })
  range.setAttribute('aria-label', 'Avatar roundness')
  range.oninput = () => {
    value.textContent = range.value + '%'
    document.documentElement.style.setProperty('--avatar-roundness', range.value / 2 + '%')
  }
  row.append(element('span', '', 'Avatar roundness'), range, value)
  form.append(row)
  toggle('showInaccessibleChannels', 'Show inaccessible channels')
  node.append(form, error)
}
$('#preferences-button').onclick = preferencesPanel
