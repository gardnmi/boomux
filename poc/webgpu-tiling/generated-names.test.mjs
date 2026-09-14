import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
 const page=await browser.newPage(),ops=[],errors=[];
 page.on('pageerror',e=>errors.push(e.message));
 const info={node_id:'local',workspace_id:'home',nodes:[],snapshot:{workspaces:[{id:'home',name:'Home',default_cwd:'/tmp',shells:[],agents:[]}]}};
 await page.route('**/api/**',r=>r.fulfill({json:{}}));
 await page.route('**/api/snapshot',r=>r.fulfill({json:info}));
 await page.route('**/api/resource',r=>{
  const op=r.request().postDataJSON().operation;ops.push(op);
  if(op.action==='create_workspace'){
   const id='created'+ops.length;info.snapshot.workspaces.push({id,name:op.name||'gentle-whale',default_cwd:'/tmp',agents:[],shells:[{id:'shell'+id,name:'ready-ember',status:'running',run:{id:'run'+id}}]});
   return r.fulfill({json:{workspace_id:id}});
  }
  return r.fulfill({json:{}});
 });
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4390');await page.locator('.workspace-button').waitFor();
 for(const name of ['', 'My project']){
  await page.locator('#header-new summary').click();await page.locator('#header-new-workspace').click();
  const dialog=page.getByRole('dialog'),input=dialog.getByLabel('Name (optional)',{exact:true});
  assert.equal(await input.getAttribute('required'),null);await input.fill(name);
  await dialog.getByRole('button',{name:'New Workspace',exact:true}).click();await dialog.waitFor({state:'detached'});
  assert.equal(ops.at(-1).name,name);
 }
 assert.deepEqual(errors,[]);console.log('Blank Workspace names submit for generation; explicit names remain unchanged');
}finally{await browser.close();}
