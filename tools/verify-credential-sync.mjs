import assert from 'node:assert/strict'
import {mkdtemp,readFile} from 'node:fs/promises'
import {tmpdir} from 'node:os'
import {join} from 'node:path'
import {createHash} from 'node:crypto'
import vm from 'node:vm'
import {CredentialSync} from '../packages/tools/credential-sync.mjs'
process.env.CUA_VA_CONFIG_DIR=await mkdtemp(join(tmpdir(),'credential-race-'))
const store=await import('../packages/config/store.mjs')
const source=(await readFile(new URL('../packages/tools/netease.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
const start=source.indexOf('export async function syncCredentialsAsync(')
const code=source.slice(start,source.indexOf('\n}',start)+2).replace('export ','')
const deferred=()=>{let resolve;const promise=new Promise(r=>resolve=r);return {promise,resolve}}
const A={appId:'A',privateKey:'key-A'},B={appId:'B',privateKey:'key-B'}
const hash=c=>createHash('sha256').update(`${c.appId}:${c.privateKey}`).digest('hex').slice(0,16)
let cases=0
for(const scenario of ['get','set','aba'])for(const cancel of [false,true]){
 const phase=scenario==='aba'?'set':scenario
 if(scenario==='aba'){await store.saveConfig({netease:{...B,syncedHash:hash(B)}});await store.saveConfig({netease:A})}
 else await store.saveConfig({netease:{...A,syncedHash:''}})
 const entered=deferred(),pause=deferred(),cli=scenario==='aba'?{...B}:{},calls=[];let held=false
 const ctx=vm.createContext({createHash,runCliAsync:async argv=>{
  const [,action,key,value]=argv;calls.push({action,key,value})
  if(action===phase&&!held){held=true;entered.resolve();await pause.promise}
  if(action==='set'){cli[key]=value;return {code:0,raw:''}}
  return {code:0,raw:cli[key]??''}
 }})
 vm.runInContext(code,ctx)
 const owner=new CredentialSync(store,ctx.syncCredentialsAsync),controller=new AbortController()
 const old=owner.sync(controller.signal)
 const oldResult=old.catch(e=>e)
 await entered.promise
 await store.saveConfig({netease:scenario==='aba'?B:{...B,syncedHash:''}})
 const latest=owner.sync()
 if(cancel)controller.abort()
 pause.resolve();await oldResult;await latest
 const config=await store.loadConfig()
 assert.deepEqual({appId:cli.appId,privateKey:cli.privateKey},B,'CLI final values must be B')
 assert.equal(config.netease.appId,'B');assert.equal(config.netease.syncedHash,hash(B))
 const count=calls.length
 await Promise.all([owner.sync(),owner.sync()]);assert.equal(calls.length,count,'same version uses successful hash cache')
 assert.equal((await owner.close()).ok,true);cases++
}
for(const failedKey of ['appId','privateKey','read-only-failure']){
 await store.saveConfig({netease:{...A,syncedHash:''}})
 let fail=true;const cli={};let writes=0
 const ctx=vm.createContext({createHash,runCliAsync:async argv=>{
  const [,action,key,value]=argv
  if(action==='get')return {code:1,raw:'failed read'}
  writes++
  if(fail&&key===failedKey)return {code:1,raw:'sensitive stderr must not be logged'}
  cli[key]=value;return {code:0,raw:''}
 }})
 vm.runInContext(code,ctx);const owner=new CredentialSync(store,ctx.syncCredentialsAsync)
 if(failedKey!=='read-only-failure'){
  await assert.rejects(owner.sync(),e=>e.code==='CREDENTIAL_SYNC_FAILED'&&!e.message.includes('sensitive'))
  assert.equal((await store.loadConfig()).netease.syncedHash,'')
 }
 fail=false;await owner.sync()
 assert.deepEqual(cli,A);assert.equal((await store.loadConfig()).netease.syncedHash,hash(A))
 const before=writes;await owner.sync();assert.equal(writes,before)
 await owner.close();cases++
}
// Store CAS is inside the serialized save, not a preceding caller-side check.
await store.saveConfig({netease:{...A,syncedHash:''}})
await Promise.all([store.saveConfig({netease:B}),assert.rejects(store.saveConfig({netease:{syncedHash:hash(A)}},{expectedCredentials:A}),e=>e.code==='STALE_CREDENTIALS')])
assert.equal((await store.loadConfig()).netease.appId,'B')
{
 const entered=deferred(),pause=deferred();let calls=0
 const owner=new CredentialSync(store,async(_c,{signal})=>{calls++;entered.resolve();await pause.promise;signal.throwIfAborted();return {hash:'invalid'}})
 const pending=owner.sync();const rejected=assert.rejects(pending)
 await entered.promise;const queued=owner.sync();const qRejected=assert.rejects(queued)
 const closing=owner.close();pause.resolve();await Promise.all([rejected,qRejected,closing])
 assert.equal(calls,1);assert.notEqual((await store.loadConfig()).netease.syncedHash,'invalid')
 assert.equal((await owner.close()).ok,true)
}
console.log(`Credential synchronization: ${cases} concurrent/failure scenarios + atomic CAS + shutdown PASS (temporary config, mock CLI final values asserted)`)
{
 const entered=deferred(),pause=deferred();let calls=0
 const owner=new CredentialSync(store,async(_c,{signal})=>{calls++;if(calls===1){entered.resolve();await pause.promise}signal.throwIfAborted();return {hash:hash(B)}})
 const first=owner.sync(),r1=assert.rejects(first);await entered.promise
 const queued=owner.sync(),r2=assert.rejects(queued)
 owner.cancelPending();pause.resolve();await Promise.all([r1,r2])
 await owner.sync();assert.equal(calls,2,'new generation can synchronize without reviving cancelled queued work')
 await owner.close()
}
console.log('HUD cancellation invalidates old queue and permits a new generation: PASS')
