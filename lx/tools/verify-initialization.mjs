import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
import { AgentInitialization } from '../packages/agent/initialization.mjs'
const source=(await readFile(new URL('../apps/shell/main.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
const a=source.indexOf('async function ensureAgent(signal)')
assert.ok(a>=0)
const code=source.slice(a,source.indexOf('\n}',a)+2).replaceAll('import(', 'loadModule(')
const deferred=()=>{let resolve,reject;const promise=new Promise((r,j)=>{resolve=r;reject=j});return {promise,resolve,reject}}
const tick=()=>new Promise(r=>setImmediate(r))
let cases=0
function fixture(stage, failure=false) {
 const pause=deferred(),entered=deferred(),controllers=[],resources=[]
 const counts={bridge:0,disposed:0,sync:0,save:0,launcher:0,agent:0,active:0}
 let paused=false
 const wait=async name=>{if(stage===name && !paused){paused=true;entered.resolve();await pause.promise;if(failure)throw Error(`failed ${name}`)}}
 const store={loadConfig:async()=>{await wait('config');return {netease:{appId:'test'},model:'test'}},resolveApiKey:async()=>{await wait('key');return 'test'},resolveSearchKey:async()=>{await wait('search');return ''},saveConfig:async()=>{counts.save++}}
 class Toolkit {
  constructor(){counts.bridge++;counts.active++;resources.push(this)}
  async warmup(){await wait('warmup')}
  async dispose(){counts.disposed++;counts.active--;this.closed=true}
 }
 class Agent {constructor(){counts.agent++;if(stage==='agent-failure')throw Error('agent construction failed')}}
 const modules={
  '../../packages/config/store.mjs':store,
  '../../packages/agent/llm.mjs':{LLMClient:class{}},
  '../../packages/agent/loop.mjs':{Agent},
  '../../packages/cua/adapter.mjs':{CuaToolkit:Toolkit},
  '../../packages/tools/web-search.mjs':{WebSearch:class{enabled=false}},
 }
 const owner=new AgentInitialization()
 const ctx=vm.createContext({AgentInitialization,DOMException,Date,console,join:()=>'',DATA_ROOT:'test',
 agentInitialization:owner,shuttingDown:false,
 loadModule:async path=>{await wait(path.includes('/config/')?'modules':'other-module');return modules[path]},
 syncCredentialsAsync:async()=>{counts.sync++;await wait('music');return {synced:[],hash:'new'}},
 lxAvailable:async()=>{counts.sync++;await wait('music');return true},
 ensureLauncher:async signal=>{signal?.throwIfAborted();counts.launcher++;await wait('launcher');return {}},
 log(){},applyModelSchedule(){},handleAgentEvent(){},
 })
 vm.runInContext(code,ctx)
 const start=()=>{const c=new AbortController();controllers.push(c);return {c,p:ctx.ensureAgent(c.signal)}}
 return {owner,start,counts,pause,entered,resources}
}
// Cancellation at every asynchronous initialization stage must stop following work.
for (const stage of ['modules','config','key','music','warmup','search','launcher']) {
 const f=fixture(stage), {c,p}=f.start();const rejected=assert.rejects(p,e=>e.name==='AbortError')
 await f.entered.promise;c.abort();await rejected
 f.pause.resolve();await tick();await tick()
 assert.equal(f.owner.runtime,null);assert.equal(f.owner.loading,false)
 assert.equal(f.counts.active,0);assert.equal(f.counts.disposed,f.counts.bridge)
 if(['modules','config','key','music'].includes(stage))assert.equal(f.counts.bridge,0)
 if(stage!=='launcher')assert.equal(f.counts.launcher,0)
 if(['modules','config','key','music'].includes(stage))assert.equal(f.counts.save,0)
 cases++
}
// An already-aborted caller must not even load a module.
{
 const f=fixture('modules'),c=new AbortController();c.abort()
 await assert.rejects(f.owner.ensure(c.signal,()=>{throw Error('must not build')}),e=>e.name==='AbortError')
 assert.equal(f.owner.loading,false);assert.equal(f.counts.bridge,0);cases++
}
for(const stage of ['warmup','search','launcher','agent-failure']) {
 const f=fixture(stage,true),{p}=f.start();const rejected=assert.rejects(p)
 if(stage!=='agent-failure'){await f.entered.promise;f.pause.resolve()}
 await rejected
 assert.equal(f.counts.active,0);assert.equal(f.counts.disposed,1)
 assert.equal(f.owner.runtime,null);assert.equal(f.owner.loading,false)
 if(stage!=='agent-failure'){
  const runtime=await f.start().p;assert.equal(f.owner.runtime,runtime);assert.equal(f.counts.active,1)
 }
 await f.owner.close();assert.equal(f.counts.active,0);cases++
}
// Two consumers can share initialization. Cancelling one cannot destroy the other's resource.
{
 const f=fixture('warmup'),first=f.start();await f.entered.promise
 const second=f.start();const rejected=assert.rejects(first.p,e=>e.name==='AbortError');first.c.abort();await rejected
 assert.equal(f.counts.disposed,0)
 f.pause.resolve();const runtime=await second.p
 assert.equal(f.owner.runtime,runtime);assert.equal(f.counts.bridge,1)
 const third=await f.start().p;assert.equal(third,runtime)
 await f.owner.close();assert.equal(f.counts.active,0);assert.equal(f.counts.disposed,1);cases++
}
// A cancelled job's late finally must not clear/destroy a newer initialization.
{
 const f=fixture('warmup'),old=f.start();await f.entered.promise
 const rejected=assert.rejects(old.p);old.c.abort();await rejected
 const next=f.start();const runtime=await next.p
 f.pause.resolve();await tick();await tick()
 assert.equal(f.owner.runtime,runtime);assert.equal(runtime.toolkit.closed,undefined)
 assert.equal(f.counts.active,1);assert.equal(f.counts.disposed,1)
 await f.owner.close();assert.equal(f.counts.active,0);cases++
}
// Exit during import/warmup and at the publication boundary is terminal and resource-safe.
for(const stage of ['modules','warmup','launcher']) {
 const f=fixture(stage),{p}=f.start();const rejected=assert.rejects(p)
 await f.entered.promise
 const closing=f.owner.close();f.pause.resolve()
 await Promise.all([closing,rejected]);await tick()
 assert.equal(f.owner.runtime,null);assert.equal(f.counts.active,0)
 await assert.rejects(f.start().p,e=>e.name==='AbortError');cases++
}
// Cleanup failure retains ownership for a later exit retry.
{
 let disposed=0,diagnostics=0
 const owner=new AgentInitialization({diagnostic(){diagnostics++}})
 const controller=new AbortController()
 await assert.rejects(owner.ensure(controller.signal,async({own})=>{
  own({dispose:async()=>{disposed++;if(disposed===1)throw Error('transient cleanup failure')}})
  throw Error('initialization failed')
 }))
 assert.equal(diagnostics,1);await owner.close();assert.equal(disposed,2);cases++
}
// Cancelled imports cannot publish into a newer pending initialization.
{
 const owner=new AgentInitialization(),oldPause=deferred(),newPause=deferred()
 const oldController=new AbortController(),newController=new AbortController()
 const old=owner.ensure(oldController.signal,async({checkpoint})=>{await oldPause.promise;checkpoint();return {}})
 const rejected=assert.rejects(old);await tick();oldController.abort();await rejected
 const next=owner.ensure(newController.signal,async()=>{await newPause.promise;return {}})
 oldPause.resolve();await tick();assert.equal(owner.loading,true)
 newPause.resolve();await next;await owner.close();cases++
}
console.log(`Agent initialization ownership: ${cases} scenarios PASS (actual initializer, mocked modules/resources)`)
// Root-only credential initialization checks: never touch the actual CLI/config.
{
 const { createHash } = await import('node:crypto')
 let musicSource
 try { musicSource=await readFile(new URL('../packages/tools/netease.mjs',import.meta.url),'utf8') }
 catch(error){if(error.code!=='ENOENT')throw error}
 if(musicSource){
  musicSource=musicSource.replace(/\r\n/g,'\n')
  const start=musicSource.indexOf('export async function syncCredentialsAsync(')
  const code=musicSource.slice(start,musicSource.indexOf('\n}',start)+2).replace('export ','')
  for(const phase of ['get','set']){
   const pause=deferred(),entered=deferred(),controller=new AbortController(),calls=[]
   const ctx=vm.createContext({createHash,runCliAsync:async(argv,options)=>{
    assert.equal(options.signal,controller.signal);calls.push(argv[1])
    if(argv[1]===phase){entered.resolve();await pause.promise}
    return {raw:'',code:0}
   }})
   vm.runInContext(code,ctx)
   const pending=ctx.syncCredentialsAsync({appId:'fake',privateKey:'fake'},{signal:controller.signal})
   const rejected=assert.rejects(pending,e=>e.name==='AbortError')
   await entered.promise;controller.abort();pause.resolve();await rejected
   assert.deepEqual(calls,phase==='get'?['get']:['get','set'])
  }
  console.log('Cancelled credential sync: 2 mocked CLI scenarios PASS (no real config writes)')
 }
}
