import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
const source=(await readFile(new URL('../apps/shell/main.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
const fn=name=>{const a=source.indexOf(name);assert.ok(a>=0,name);return source.slice(a,source.indexOf('\n}',a)+2)}
const deferred=()=>{let resolve,reject;const promise=new Promise((r,j)=>{resolve=r;reject=j});return {promise,resolve,reject}}
let cases=0
for(const fallback of [false,true]) for(const phase of ['launcher-failure','launch-failure','success','cancel-launcher','cancel-launch','late-error']) {
 const pause=deferred(),entered=deferred();let launches=0
 const ctx=vm.createContext({AbortController,DOMException,process,Date,clearTimeout(){},
 ensureLauncher:async signal=>{
  assert.ok(signal instanceof AbortSignal)
  if(phase==='launcher-failure')throw Error('launcher init failed')
  if(phase==='cancel-launcher'){entered.resolve();await pause.promise}
  return {launch:async()=>{
   launches++
   if(phase==='launch-failure')throw Error('launch failed')
   if(['cancel-launch','late-error'].includes(phase)){entered.resolve();await pause.promise}
   return {text:'opened'}
  }}
 },
 refreshSearchKey:async()=>{},ensureAgent:async()=>{throw Object.assign(Error('network failure'),{network:true})},
 // runCommand 的回合入口会先核对磁盘配置；本用例不关心它，给个空实现
 syncAgentEndpointIfChanged:async()=>{},
 })
 vm.runInContext(`
 const musicCredentials={cancelPending(){}};let turnSeq=0,activeTurn=null,latestTurn=null,hideTimer=null,commandTimer='timer',commandActive=false;
 let wantContinue=false,followUpJudgement=null,followUpJudgePromise=null,asr=null,lastCommandText='',lastSentPartial='';
 let lastUtterance='',deniedInTurn={},turnHistoryCounts=[],sessionHistory=[],userDismissedHud=false,listening=false;
 let ui=[],closed=0,after=0;
 function closeConfirmCard(){closed++} function stopSpeaking(){} function hideHud(){ui.push('hide')}
 function log(){} function sendSubtitle(x){ui.push(x)} function setState(x){ui.push(x)} function scheduleHide(){ui.push('scheduled')}
 function replyOffline(text){ui.push(text)} function afterTurnSpeechSettled(){after++}
 function parseOfflineIntent(){return {intent:'launch',arg:'test'}} function isOfflineNow(){return ${!fallback}}
 function isOfflineError(error){return error.network===true} function offlineUnsupported(){return 'unsupported'}
 ${fn('function cancelActiveTurn(reason)')}
 ${fn('function isCurrentTurn(turn)')}
 ${fn('function dismiss()')}
 ${fn('async function runOfflineCommand(text, turn)')}
 ${fn('async function runCommand(text)')}
 `,ctx)
 const run=code=>vm.runInContext(code,ctx)
 const pending=run("runCommand('open test')")
 if(phase.startsWith('cancel') || phase==='late-error') {
  await entered.promise
  run('dismiss();userDismissedHud=false;var newer={id:++turnSeq,controller:new AbortController()};activeTurn=latestTurn=newer')
  const ui=run('JSON.stringify(ui)'),closed=run('closed')
  if(phase==='late-error')pause.reject(Error('late failure'));else pause.resolve()
  await pending
  assert.equal(run('activeTurn===newer'),true)
  assert.equal(run('closed'),closed)
  assert.equal(run('JSON.stringify(ui)'),ui)
  assert.equal(launches,phase==='cancel-launcher'?0:1)
 } else {
  await pending // A fallback failure must resolve through error reporting, never reject unhandled.
  assert.equal(run('activeTurn'),null)
  assert.equal(run('commandActive'),false)
  assert.equal(run('closed'),2)
  if(phase==='success'){
   assert.equal(run('after'),1)
   assert.equal(run('ui.includes("opened")'),true)
  }else{
   assert.equal(run('ui.includes("error")'),true)
   assert.equal(run('after'),0)
  }
 }
 cases++
}
console.log(`Offline turn cleanup: ${cases} scenarios PASS (actual shell paths, mocked launcher)`)
