import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:900}}),errors=[],ops=[];
 page.on('pageerror',e=>errors.push(e.message));
 const home={id:'home',name:'Home',shells:[],agents:[]};
 const info={node_id:'local',workspace_id:'home',nodes:[],snapshot:{workspaces:[home]}};
 let setup,serial=0;
 await page.route('**/api/snapshot',r=>r.fulfill({json:info}));
 await page.route('**/api/changes',r=>r.fulfill({status:404}));
 await page.route('**/api/desktop',r=>r.fulfill({json:{projects:[]}}));
 await page.route('**/api/attach',r=>r.fulfill({status:409,json:{error:'shell already has an active controller; use takeover'}}));
 await page.route('**/api/resource',r=>{
  const op=r.request().postDataJSON().operation;ops.push(op);
  if(op.action==='guided'){
   serial++;const shell={id:'setup-shell'+serial,name:'setup',cwd:'/tmp',workspace_id:'setup'+serial,run:null};setup={id:shell.workspace_id,name:'Setup '+serial,shells:[shell],agents:[]};info.snapshot.workspaces.push(setup);
   return r.fulfill({json:{shell,workspace_id:setup.id,setup_token:'token'+serial}});
  }
  if(op.action==='start_shell'){setup.shells[0].run={id:'run'};return r.fulfill({json:{shell:setup.shells[0]}});}
  if(op.action==='finish_setup'){
   const shell={id:'remote-shell'+serial,name:'created',cwd:'/remote',workspace_id:'remote'+serial,run:{id:'remote-run'}};
   info.snapshot.workspaces=info.snapshot.workspaces.filter(w=>w.id!==setup.id);
   info.snapshot.workspaces.push({id:shell.workspace_id,name:'Remote '+serial,shells:[shell],agents:[]});
   return r.fulfill({json:{pending:false,shell}});
  }
 });
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4387');await page.waitForSelector('.workspace-button');
 async function start(){await page.locator('#header-new summary').click();await page.locator('#header-connect').click();await page.getByRole('dialog').getByRole('button',{name:'Connect another machine…'}).click();await page.waitForFunction(()=>document.querySelector('.workspace-group.current')?.textContent.includes('Setup'));await page.getByRole('button',{name:'Take control',exact:true}).first().waitFor();}
 async function refresh(){await page.locator('#header-more summary').click();await page.locator('#header-refresh').click();}
 await start();setup.shells=[];await refresh();await page.waitForFunction(()=>document.querySelector('.workspace-group.current')?.textContent.includes('Remote 1'));
 assert.equal(ops.filter(op=>op.action==='finish_setup').length,1);
 await start();await page.locator('.workspace-button').filter({hasText:'Home'}).click();setup.shells=[];await refresh();
 await page.waitForFunction(()=>[...document.querySelectorAll('.workspace-button')].some(el=>el.textContent.includes('Remote 2')));
 assert.ok((await page.locator('.workspace-group.current').textContent()).includes('Home'),'finishing setup does not steal navigation from a different Workspace');
 assert.equal(ops.filter(op=>op.action==='finish_setup').length,2);assert.deepEqual(errors,[]);
 console.log('Exact remote setup handoff, single completion consumption, and navigation preservation passed');
}finally{await browser.close();}
