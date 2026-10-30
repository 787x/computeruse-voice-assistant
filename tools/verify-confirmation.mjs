import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
process.env.CUA_VA_AUTO_CONFIRM = ''
import {parseConfirmText,PermissionGate} from '../packages/agent/permissions.mjs'
const source = await readFile(new URL('../apps/shell/main.mjs', import.meta.url), 'utf8').then(s => s.replace(/\r\n/g, '\n'))
function section(start, end) { return source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start))) }
const timers = []
const ctx = vm.createContext({ console, process, parseConfirmText, setTimeout: fn => (timers.push(fn), timers.length), clearTimeout() {} })
vm.runInContext(`
let confirmResolver=null, confirmSequence=0, pendingConfirmId=null, pendingConfirmCapability=null;
let commandActive=false, commandTimer=null, asr={reset(){},flush(){return '确认吗？'}}, lastCommandText='',lastSentPartial='';
let listening=true; let confirmVoiceEpoch=0; const confirmVoiceSource=Symbol('ASR'); let voiceConfirmEnabled=true, heardAny=false, commandStartAt=0, hideTimer=null, activeTurn=null,currentState='hidden';
let followUpWindowStart=0, grants=0, permissionGate={grantOnce(){grants++;return Date.now()}};
let win={isDestroyed:()=>false,webContents:{send(){}, mainFrame:{}}};
const CONFIRM_TIMEOUT_MS=12000;
function describeRequest(){return {text:'test',detail:'test'}}
function stopSpeaking(){} function setState(s){currentState=s} function sendSubtitle(){} function log(){}
${section('function setVoiceConfirmEnabled(enabled)', '\n/**')}
${section('async function askUser(request)', '/**\n * 收掉卡片')}
${section('function closeConfirmCard()', '\n}').concat('\n}')}
${section('function onWake(keyword)', '\n}').concat('\n}')}
${section('function finishCommand(reason, voiceEpoch = confirmVoiceEpoch, endpointText = null, requestId = pendingConfirmId)', '\n}').concat('\n}')}
let ipcHandler;
const ipcMain={on(name,handler){ipcHandler=handler}};
${section("  ipcMain.on('hud:confirm',", '\n\n  // 麦克风')}
`, ctx)
const run = code => vm.runInContext(code,ctx)
run("var p=askUser({capability:'clipboard.read'}); var first=pendingConfirmId")
run("onWake('wake'); finishCommand('endpoint')") // ambiguous real parser result must leave the card usable
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
run("var u=askUser({}); voiceConfirmEnabled=true; settleConfirm({allow:false,channel:'voice',epoch:confirmVoiceEpoch},pendingConfirmId,confirmVoiceSource)")
assert.equal((await run('u')).allow,false)
console.log('Confirmation lifecycle: PASS (real parser, disabled voice, wake, stale timeout/click, sender, repeat, grant, close, timeout, cancel)')

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

vm.runInContext(section('function feedCommandAudio(chunk)', '\n}').concat('\n}'),ctx)
for (const text of ['可以','确认','同意','好的','我同意','我确认','可以的','好啊','不可以','不同意','不确认','不允许','不对','错了','停','不用','别']) {
 run(`listening=true;voiceConfirmEnabled=true;asr={reset(){},flush(){return ''},push(){return {partial:'',final:${JSON.stringify(text)}}}}`)
 const gate=new PermissionGate({confirm:request=>run('askUser({capability:"clipboard.read"})')})
 const pending=gate.check({tool:'read_clipboard'})
 run("commandStartAt=0;feedCommandAudio([])")
 const verdict=await pending
 assert.equal(verdict.allow,parseConfirmText(text).allow,text)
 assert.equal(verdict.channel,'voice')
}
run("var partial=askUser({});asr.push=()=>({partial:'可以',final:null});commandStartAt=0;feedCommandAudio([])")
assert.equal(run('confirmResolver!==null'),true,'partial must never approve')
run("asr.push=()=>({partial:'',final:'不可以'});feedCommandAudio([])")
assert.equal((await run('partial')).allow,false)
run("var emptyFlush=askUser({});lastCommandText='可以';asr.flush=()=>'';finishCommand('timeout')")
assert.equal(run('confirmResolver!==null'),true,'empty flush must not reuse partial')
run("asr.flush=()=> '可以';finishCommand('timeout')")
assert.equal((await run('emptyFlush')).allow,true,'valid flush must preserve voice approval')
run("var oldRequest=askUser({});var oldId=pendingConfirmId,oldEpoch=confirmVoiceEpoch;var replacement=askUser({})")
assert.equal((await run('oldRequest')).allow,false)
run("finishCommand('endpoint',oldEpoch,'可以',oldId)")
assert.equal(run('confirmResolver!==null'),true)
run("settleConfirm({allow:true,channel:'voice',epoch:confirmVoiceEpoch},pendingConfirmId,true)")
assert.equal(run('confirmResolver!==null'),true,'a claimed voice channel is not local ASR authority')
run("closeConfirmCard();finishCommand('endpoint',oldEpoch,'可以',oldId)")
assert.equal((await run('replacement')).allow,false)
console.log('Real parser + ASR final/partial/flush + PermissionGate voice approval and stale-source isolation: PASS')

run("var switched=askUser({});var switchId=pendingConfirmId;setVoiceConfirmEnabled(false)")
const switchTimers=timers.length
for(const text of ['可以','取消']) {
 run(`asr.push=()=>({partial:'',final:${JSON.stringify(text)}});commandActive=true;commandStartAt=0;feedCommandAudio([])`)
 assert.equal(run('confirmResolver!==null'),true,'disabled voice ignores both answers')
}
run("setVoiceConfirmEnabled(true);asr.push=()=>({partial:'',final:'可以'});commandStartAt=0;feedCommandAudio([])")
assert.equal((await run('switched')).allow,true)
assert.equal(timers.length,switchTimers)
console.log('Off/on restores one-utterance voice approval without extending timeout: PASS')

for(const text of ['我同意吗？','我说的是可以','我说的是不可以','不是不可以','不要取消','如果安全就可以','“我同意”','嗯']) {
 run(`var ambiguous=askUser({});asr.push=()=>({partial:'',final:${JSON.stringify(text)}});commandStartAt=0`)
 const before=timers.length
 run('feedCommandAudio([])')
 assert.equal(run('confirmResolver!==null'),true,text)
 assert.equal(timers.length,before,'ambiguous speech must not extend timeout')
 timers.at(-1)()
 assert.equal((await run('ambiguous')).channel,'timeout')
}
console.log('Natural confirmations/refusals and ambiguous card/deadline preservation: PASS')

// Preserve the existing explicit automation hook, isolated from speech and old requests.
process.env.CUA_VA_AUTO_CONFIRM='allow'
run('var automaticOld=askUser({});var autoOldCallbackIndex=0')
const oldAuto=timers.at(-2)
run('var automaticNew=askUser({})')
assert.equal((await run('automaticOld')).allow,false)
oldAuto()
assert.equal(run('confirmResolver!==null'),true,'old automation callback must not approve the new card')
timers.at(-2)()
assert.equal((await run('automaticNew')).channel,'auto-test')
process.env.CUA_VA_AUTO_CONFIRM=''
assert.ok(source.includes('Number(process.env.CUA_VA_CONFIRM_TIMEOUT ?? 180000)'))
console.log('Existing explicit auto-test hook stays request-scoped and distinct from ASR: PASS')
