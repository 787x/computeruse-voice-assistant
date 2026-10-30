import assert from 'node:assert/strict'
import {readFile} from 'node:fs/promises'
import vm from 'node:vm'
const source=(await readFile(new URL('../apps/shell/main.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
const fn=name=>{const a=source.indexOf(name);assert.ok(a>=0,name);return source.slice(a,source.indexOf('\n}',a)+2)}
const ctx=vm.createContext({Date,setTimeout:()=>1,clearTimeout(){}})
vm.runInContext(`
let confirmResolver=null,turnSeq=0,listening=false,settingsWin=null,wakeMutedUntil=0;
let commandActive=false,heardAny=false,lastCommandText='',lastSentPartial='',commandStartAt=0,commandTimer=null;
let asrLoading=null,confirmVoiceEpoch=0,pendingConfirmId=null,followUpWindowStart=0;
const COMMAND_TIMEOUT_MS=15000,WAKE_TAIL_MS=300;
let subtitles=[],shown=[],commands=[],started=0,stopped=0;
let asr={reset(){},flush(){return ''},push(){return {partial:'现在几',final:null}}};
function log(){} function resetSession(){} function stopSpeaking(){stopped++}
function showForInteraction(state){shown.push(state)} function showForWake(state){shown.push(state)}
function sendSubtitle(x){subtitles.push(x)} function setState(){} function scheduleHide(){}
function stripBargeWords(x){return x} function runCommand(x){commands.push(x)}
async function startListening(){started++;listening=true;return {ok:true}}
${fn('async function startManualConversation()')}
${fn('function onWake(keyword, manual = false)')}
${fn('function finishCommand(reason, voiceEpoch = confirmVoiceEpoch, endpointText = null, requestId = pendingConfirmId)')}
${fn('function feedCommandAudio(chunk)')}
`,ctx)
const run=x=>vm.runInContext(x,ctx)
await run('startManualConversation()')
assert.equal(run('listening && commandActive'),true)
assert.equal(run('started'),1)
run('feedCommandAudio([])')
assert.equal(run("subtitles.some(x=>x.partial==='现在几')"),true,'manual open must stream actual ASR partial into subtitles')
run("asr.push=()=>({partial:'',final:'现在几点'});feedCommandAudio([])")
assert.equal(run("commands[0]"),'现在几点')
run('confirmResolver=()=>{};commandActive=false')
await run('startManualConversation()')
assert.equal(run('commandActive'),false)
assert.equal(run('shown.at(-1)'),'confirm')
run('confirmResolver=null;settingsWin={isDestroyed:()=>false,isVisible:()=>true};commandActive=false')
run("onWake('小爱同学')")
assert.equal(run('commandActive'),false,'automatic wake must preserve settings guard')
await run('startManualConversation()')
assert.equal(run('commandActive'),true,'explicit manual request can start while auto wake is muted')
let release
ctx.wait=new Promise(r=>release=r)
run('listening=false;commandActive=false;startListening=async()=>{await wait;return {ok:true}}')
const old=run('startManualConversation()');run('turnSeq++');release();await old
assert.equal(run('commandActive'),false,'cancelled microphone initialization cannot arm an old manual request')
assert.ok(source.includes("tray.on('click', () => { void startManualConversation() })"))
console.log('Manual conversation: microphone start, partial/final, pending confirmation, auto/manual guards and cancellation PASS')
