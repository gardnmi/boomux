// Only this test's fresh resources on the explicitly isolated daemon are mutated.
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
assert.ok(process.env.BOOMUX_RUNTIME_DIR?.endsWith('/target/webgpu-poc/runtime'),'Select the isolated daemon');
const base=process.env.POC_URL||'http://127.0.0.1:4389',cli=process.env.POC_BOOMUX_BIN;
assert.ok(cli,'Select POC_BOOMUX_BIN');
const info=await (await fetch(base+'/api/snapshot')).json();
const local=JSON.parse(execFileSync(cli,['node','snapshot','--json'],{encoding:'utf8'})).data.nodes.find(n=>n.local);
assert.equal(local.node_id,info.node_id);
async function post(path,body){const response=await fetch(base+path,{method:'POST',headers:{Origin:base,'Content-Type':'application/json'},body:JSON.stringify({node_id:info.node_id,...body})});const result=await response.json();assert.ok(response.ok,JSON.stringify(result));return result;}
const action=operation=>post('/api/resource',{operation});
let id;
try{
 const baseline=await post('/api/changes',{cursor:null});
 id=(await action({action:'create_workspace',name:'web-parity-fixture-'+Date.now(),cwd:'/tmp'})).workspace_id;
 const changed=await post('/api/changes',{cursor:baseline.cursor});assert.equal(changed.changed,true,'metadata changes invalidate the browser snapshot');
 const shell=(await post('/api/shell',{workspace_id:id})).shell;
 await action({action:'rename',id:shell.id,workspace:false,name:'renamed-fixture-shell'});
 await action({action:'rename',id,workspace:true,name:'renamed-fixture-workspace'});
 const snapshot=await (await fetch(base+'/api/snapshot')).json();const workspace=snapshot.snapshot.workspaces.find(w=>w.id===id);
 assert.equal(workspace.name,'renamed-fixture-workspace');assert.equal(workspace.shells[0].name,'renamed-fixture-shell');
 const wrong=await fetch(base+'/api/resource',{method:'POST',headers:{Origin:base,'Content-Type':'application/json'},body:JSON.stringify({node_id:'wrong',operation:{action:'remove_workspace',id}})});assert.equal(wrong.status,409);
 await action({action:'remove_workspace',id});id=null;
 console.log('Isolated Workspace/Shell creation, guarded rename/removal, owner rejection and event invalidation passed');
}finally{if(id)await action({action:'remove_workspace',id});}
