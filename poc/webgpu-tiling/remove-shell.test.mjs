// Mock all daemon traffic: this test never removes real Shells.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 for(const remote of [false,true]){
  const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[],requests=[];let reject=true;
  page.on('pageerror',e=>errors.push(e.message));
  const shell={id:remote?'remote:owner:shell':'shell',workspace_id:remote?'remote:owner:workspace':'workspace',name:'build',cwd:'/project',status:'running',run:{id:'run'}};
  const workspace={id:shell.workspace_id,name:'project',shells:[shell],agents:[],...(remote?{remote:{alias:'remote',current:true,stale:false,health:'online'}}:{})};
  await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:workspace.id,nodes:[],snapshot:{workspaces:[workspace]}}}));
  await page.route('**/api/attach',r=>r.fulfill({json:{token:'fixture'}}));
  await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('persistent Shell\r\n'));});
  await page.route('**/api/shell/remove',r=>{
   requests.push(r.request().postDataJSON());
   if(reject)return r.fulfill({status:409,json:{error:'ShellRun changed; refresh and confirm removal again'}});
   workspace.shells=[];return r.fulfill({json:{removed:true}});
  });
  await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
  await page.waitForSelector('.pane-body[data-connected="true"]');
  page.once('dialog',d=>d.dismiss());await page.getByRole('button',{name:'Remove Shell',exact:true}).click();
  assert.equal(requests.length,0,'cancel makes no removal request');assert.equal(await page.locator('.pane').count(),1);
  page.once('dialog',d=>d.accept());await page.getByRole('button',{name:'Remove Shell',exact:true}).click();
  await page.getByText('ShellRun changed; refresh and confirm removal again',{exact:true}).waitFor();
  assert.equal(await page.locator('.pane').count(),1,'failure retains the pane');
  assert.deepEqual(requests[0],{node_id:'local',shell_id:shell.id,run_id:'run'});
  await page.getByRole('button',{name:'Minimize pane',exact:true}).click();assert.equal(await page.locator('.pane').count(),0);
  assert.equal(requests.length,1,'minimize never removes the Shell');
  await page.locator('.shell-button').click();await page.waitForSelector('.pane-body[data-connected="true"]');
  reject=false;page.once('dialog',d=>d.accept());await page.getByRole('button',{name:'Remove Shell',exact:true}).click();
  await page.waitForFunction(()=>document.querySelectorAll('.pane').length===0&&document.querySelectorAll('.shell-button').length===0);
  assert.equal(requests.length,2);assert.deepEqual(errors,[]);await page.close();
 }
 console.log('Local/remote exact identity, cancel, failed removal, minimize separation, and successful removal passed');
}finally{await browser.close();}
