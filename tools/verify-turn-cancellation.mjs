import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
const source=(await readFile(new URL('../apps/shell/main.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
const fn=name=>{const a=source.indexOf(name);assert.ok(a>=0,name);return source.slice(a,source.indexOf('\n}',a)+2)}
const deferred=()=>{let resolve,reject;const promise=new Promise((r,j)=>{resolve=r;reject=j});return {promise,resolve,reject}}
for (const phase of ['refresh','init','model','model-error']) {
 const pause=deferred(), entered=deferred(), runDone=deferred();let runOptions, runCount=0
 const ctx=vm.createContext({AbortController,console,process,Date,clearTimeout(){},
 refreshSearchKey:async()=>{if(phase==='refresh'){entered.resolve();await pause.promise}},
 ensureAgent:async()=>{if(phase==='init'){entered.resolve();await pause.promise}; return {agent:{run:async(_text,options)=>{
 runOptions=options;runCount++;entered.resolve();return await runDone.promise
 }}}}})
 vm.runInContext(`
 const musicCredentials={cancelPending(){}};let turnSeq=0,activeTurn=null,latestTurn=null,hideTimer=null,commandTimer=null,commandActive=false;
 let wantContinue=false,followUpJudgement=null,followUpJudgePromise=null,asr=null,lastCommandText='',lastSentPartial='';
 let lastUtterance='',deniedInTurn={},turnHistoryCounts=[],sessionHistory=[],userDismissedHud=false,listening=false;
 let ui=[],closed=0;
 function closeConfirmCard(){closed++} function stopSpeaking(){} function hideHud(){ui.push('hide')}
 function log(){} function sendSubtitle(x){ui.push(x)} function setState(x){ui.push(x)}
 function parseOfflineIntent(){return null} function isOfflineNow(){return false}
 function takeSentences(){return []} function flushTurn(){ui.push('flush')} function speakLine(){ui.push('speak')}
 ${fn('function cancelActiveTurn(reason)')}
 ${fn('function isCurrentTurn(turn)')}
 ${fn('function dismiss()')}
 ${fn('function handleAgentEvent(event, turn = activeTurn)')}
 ${fn('async function runCommand(text)')}
 `,ctx)
 const run=code=>vm.runInContext(code,ctx)
 const old=run("runCommand('old')")
 await entered.promise
 const before=run('activeTurn.controller.signal')
 run('dismiss()')
 assert.equal(before.aborted,true,'dismiss must abort owned turn')
 if(phase.startsWith('model')){
   const snapshot=run('JSON.stringify(ui)')
   runOptions.onEvent({type:'text_delta',text:'late'})
   assert.equal(run('JSON.stringify(ui)'),snapshot,'old deltas must not write UI')
 }
 // A new turn must survive the old turn's late finally, completion or exception.
 run('userDismissedHud=false; var newer={id:++turnSeq,controller:new AbortController()}; activeTurn=latestTurn=newer')
 const count=run('closed'),snapshot=run('JSON.stringify(ui)')
 pause.resolve();if(phase==='model-error')runDone.reject(Error('late model error'));else runDone.resolve({text:'late',steps:[]})
 await old
 assert.equal(run('activeTurn===newer'),true)
 assert.equal(run('closed'),count,'old finally must not close new confirmation')
 assert.equal(run('JSON.stringify(ui)'),snapshot)
 assert.equal(runCount,phase.startsWith('model')?1:0)
}
console.log('Shell turn cancellation: 4 asynchronous phases PASS (dismiss, late UI/results, new-turn isolation)')

{
 const reply=deferred()
 const ctx=vm.createContext({resolveFollowUpJudgement:()=>reply.promise})
 vm.runInContext(`let turnSeq=1,continuousMode=true,listening=true,activeTurn=null,confirmResolver=null,commandActive=false,userDismissedHud=false;
 ${fn('async function startFollowUpIfWanted()')}`,ctx)
 const pending=vm.runInContext('startFollowUpIfWanted()',ctx)
 vm.runInContext('turnSeq++;userDismissedHud=true',ctx)
 reply.resolve({wait:true,confidence:1})
 assert.equal(await pending,false)
 assert.equal(vm.runInContext('commandActive',ctx),false)
 console.log('Late follow-up judgement after close: PASS')
}
// Recovery must not launch/retry a tool after the owning turn has been cancelled.
for (const phase of ['first-call','recovery-launch']) {
 const controller=new AbortController(),pause=deferred(),entered=deferred();let calls=0,launches=0
 const toolkit={invoke:async()=>{calls++;if(phase==='first-call'){entered.resolve();await pause.promise}throw Error('No such application')}}
 const launcher={launch:async()=>{launches++;entered.resolve();await pause.promise;return {text:'已启动'}}}
 const ctx=vm.createContext({setTimeout:r=>r(),log(){}})
 vm.runInContext(fn('async function invokeCuaWithRecovery(toolkit, name, args, launcher, signal)'),ctx)
 const pending=ctx.invokeCuaWithRecovery(toolkit,'get_app_state',{app:'test'},launcher,controller.signal)
 const rejected=assert.rejects(pending,e=>e.name==='AbortError')
 await entered.promise;controller.abort();pause.resolve();await rejected
 assert.equal(calls,1)
 assert.equal(launches,phase==='first-call'?0:1)
}
console.log('Cancelled CUA recovery/retry: PASS (mock toolkit/launcher)')
