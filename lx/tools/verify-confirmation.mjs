import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
const source = await readFile(new URL('../apps/shell/main.mjs', import.meta.url), 'utf8').then(s => s.replace(/\r\n/g, '\n'))
function section(start, end) { return source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start))) }
const timers = []
const ctx = vm.createContext({ console, setTimeout: fn => (timers.push(fn), timers.length), clearTimeout() {} })
vm.runInContext(`
let confirmResolver=null, confirmSequence=0, pendingConfirmId=null, pendingConfirmCapability=null;
let commandActive=false, commandTimer=null, asr={reset(){},flush(){return '确认吗？'}}, lastCommandText='',lastSentPartial='';
let listening=true; let confirmVoiceEpoch=0; let voiceConfirmEnabled=true, heardAny=false, commandStartAt=0, hideTimer=null, activeTurn=null,currentState='hidden';
let followUpWindowStart=0, grants=0, permissionGate={grantOnce(){grants++;return Date.now()}};
let win={isDestroyed:()=>false,webContents:{send(){}, mainFrame:{}}};
const CONFIRM_TIMEOUT_MS=12000;
function describeRequest(){return {text:'test',detail:'test'}}
function stopSpeaking(){} function setState(s){currentState=s} function sendSubtitle(){} function log(){}
function parseConfirmText(){return {allow:true,channel:'voice'}}
${section('function setVoiceConfirmEnabled(enabled)', '\n/**')}
${section('async function askUser(request)', '/**\n * 收掉卡片')}
${section('function closeConfirmCard()', '\n}').concat('\n}')}
${section('function onWake(keyword)', '\n}').concat('\n}')}
${section('function finishCommand(reason, voiceEpoch = confirmVoiceEpoch)', '\n}').concat('\n}')}
let ipcHandler;
const ipcMain={on(name,handler){ipcHandler=handler}};
${section("  ipcMain.on('hud:confirm',", '\n\n  // 麦克风')}
`, ctx)
const run = code => vm.runInContext(code,ctx)
run("var p=askUser({capability:'clipboard.read'}); var first=pendingConfirmId")
run("onWake('wake'); finishCommand('endpoint')") // injected affirmative parser must leave card usable
assert.equal(run('confirmResolver !== null && commandActive'),true)
run("settleConfirm({allow:true,channel:'voice',once:true})")
assert.equal(run('grants'),0)
run("voiceConfirmEnabled=false; onWake('wake'); settleConfirm({allow:false,channel:'voice',epoch:confirmVoiceEpoch})")
assert.equal(run('confirmResolver !== null'),true)
run("ipcHandler({sender:{},senderFrame:win.webContents.mainFrame},{id:first,answer:true})")
assert.equal(run('confirmResolver !== null'),true)
run("var q=askUser({capability:'clipboard.read'}); var second=pendingConfirmId")
assert.equal((await run('p')).allow,false)
timers[0]() // stale timeout cannot cancel replacement
run("ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:first,answer:true})")
assert.equal(run('confirmResolver !== null'),true)
run("ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:second,answer:'once'})")
assert.equal((await run('q')).allow,true)
run("ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:second,answer:'once'})")
assert.equal(run('grants'),1)
run("var r=askUser({}); closeConfirmCard()")
assert.equal((await run('r')).allow,false)
run('var t=askUser({})'); timers.at(-1)()
assert.equal((await run('t')).channel,'timeout')
run("var u=askUser({}); voiceConfirmEnabled=true; settleConfirm({allow:false,channel:'voice',epoch:confirmVoiceEpoch})")
assert.equal((await run('u')).allow,false)
console.log('Confirmation lifecycle: PASS (faulty parser, disabled voice, wake, stale timeout/click, sender, repeat, grant, close, timeout, cancel)')

// Setting changes reset recognition without replacing the request or its deadline.
run("var toggle=askUser({}); var toggleId=pendingConfirmId; var epoch=confirmVoiceEpoch; lastCommandText='取消'; lastSentPartial='取消'")
const deadline = timers.at(-1)
const timerCount = timers.length
run('setVoiceConfirmEnabled(false)')
assert.equal(run("commandActive===false && lastCommandText==='' && lastSentPartial===''") ,true)
assert.equal(run('pendingConfirmId'),run('toggleId'))
run('setVoiceConfirmEnabled(true)')
assert.equal(run('commandActive'),true)
assert.equal(timers.length,timerCount, 'toggle must not reset confirmation timeout')
run("settleConfirm({allow:false,channel:'voice',epoch}, toggleId); finishCommand('endpoint',epoch)")
assert.equal(run('confirmResolver !== null'),true,'stale voice result must not settle after off/on')
run('listening=false; setVoiceConfirmEnabled(false); setVoiceConfirmEnabled(true)')
assert.equal(run('commandActive'),false,'no microphone: clicks must remain available')
deadline()
assert.equal((await run('toggle')).channel,'timeout')
// Invalid payloads, frames and IDs cannot grant temporary permission.
run('var invalid=askUser({capability:"clipboard.read"}); var invalidId=pendingConfirmId')
for (const payload of ['null','{}','{id:invalidId,answer:"yes"}','{id:String(invalidId),answer:true}','{id:invalidId-1,answer:"once"}']) {
  run(`ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},${payload})`)
  assert.equal(run('confirmResolver !== null'),true)
}
run('ipcHandler({sender:win.webContents,senderFrame:{}},{id:invalidId,answer:"once"})')
assert.equal(run('confirmResolver !== null'),true)
run('ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:invalidId,answer:false})')
assert.equal((await run('invalid')).allow,false)
const controller=new AbortController()
ctx.controller=controller
run('var aborted=askUser({signal:controller.signal})')
controller.abort()
assert.equal((await run('aborted')).channel,'cancelled')
assert.equal(run('confirmResolver'),null)
run('var alreadyAborted=askUser({signal:controller.signal})')
assert.equal((await run('alreadyAborted')).allow,false)
console.log('Dynamic voice settings / invalid IPC / aborted confirmation: PASS')

run('var noAsr=askUser({}); var savedAsr=asr; asr=null; setVoiceConfirmEnabled(false); setVoiceConfirmEnabled(true)')
assert.equal(run('commandActive'),false)
run('ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:pendingConfirmId,answer:true})')
assert.equal((await run('noAsr')).allow,true,'ASR unavailable must not break clicks')
run('asr=savedAsr')
assert.ok(source.includes('setVoiceConfirmEnabled(payload.ui.voiceConfirmEnabled)'))
assert.ok(source.includes('setVoiceConfirmEnabled(item.checked)'))
