import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try {
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[],operations=[];
 page.on('pageerror',e=>errors.push(e.message));
 const info={node_id:'local',workspace_id:'one',nodes:[{id:'local',local:true},{id:'remote',alias:'omarchy',health:'online',current:true,stale:false,registration_revision:7}],snapshot:{workspaces:[{id:'one',name:'project',shells:[],agents:[]},{id:'two',name:'second',shells:[],agents:[]}]}};
 const entry={agent_id:'agent',external_session_id:'session',integration:'codex',title:'Fix parser',updated_at_ms:Date.now(),resumable:true};
 let fail=true,hold=false,release;
 await page.route('**/api/snapshot',r=>r.fulfill({json:info}));
 await page.route('**/api/changes',r=>r.fulfill({status:404}));
 await page.route('**/api/attach',r=>r.fulfill({status:409,json:{error:'shell already has an active controller; use takeover'}}));
 await page.route('**/api/desktop',r=>{const q=r.request().postDataJSON();return r.fulfill({json:q.workspace_id?{conversations:q.workspace_id==='one'?[entry]:[{...entry,agent_id:'second-agent',title:'Second conversation'}]}:{projects:[{name:'project',path:'/project'}],warnings:[]}});});
 await page.route('**/api/resource',async r=>{
  const op=r.request().postDataJSON().operation;operations.push(op);
  if(op.action==='open_conversation'){
   if(fail){fail=false;return r.fulfill({status:503,json:{error:'Temporary failure'}});}
   if(hold)await new Promise(resolve=>release=resolve);
   return r.fulfill({json:{shell:{id:op.shell_id,name:'resumed',cwd:'/project',run:{id:'run'},workspace_id:op.workspace_id}}});
  }
  if(op.action==='create_workspace'){info.snapshot.workspaces.push({id:'new',name:op.name,shells:[],agents:[]});return r.fulfill({json:{workspace_id:'new'}});}
  return r.fulfill({json:{ok:true}});
 });
 await page.route('**/api/shell',r=>r.fulfill({status:409,json:{error:'fixture: no real Shell is created'}}));
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4387');
 await page.getByRole('button',{name:'Workspace conversations',exact:true}).click();
 const panel=page.locator('#conversations-panel');
 await panel.getByText('Fix parser',{exact:true}).waitFor();
 await panel.getByRole('button',{name:'Pin',exact:true}).click();
 assert.ok(await panel.getByRole('button',{name:'Unpin',exact:true}).isVisible());
 await panel.getByRole('button',{name:'Archive',exact:true}).click();
 assert.equal(await panel.locator('.conversation-card').count(),0);
 await panel.getByRole('button',{name:'Archived',exact:true}).click();
 await panel.getByRole('button',{name:'Restore',exact:true}).click();
 await panel.getByRole('button',{name:'Recent',exact:true}).click();
 await panel.getByLabel('Search conversations').fill('missing');
 assert.equal(await panel.locator('.conversation-card').count(),0);
 await panel.getByLabel('Search conversations').fill('parser');
 await panel.getByRole('button',{name:'Resume',exact:true}).click();
 await panel.getByText('Temporary failure',{exact:true}).waitFor();
 hold=true;await panel.getByRole('button',{name:'Resume',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('.conversation-actions button')?.disabled);
 await page.locator('.workspace-button').filter({hasText:'second'}).click();
 await panel.getByText('Second conversation',{exact:true}).waitFor();
 release();await page.waitForTimeout(150);
 const resumes=operations.filter(op=>op.action==='open_conversation');
 assert.equal(resumes.length,2);assert.equal(resumes[0].shell_id,resumes[1].shell_id,'retry preserves the idempotent attempt');
 assert.equal(resumes[1].workspace_id,'one');assert.equal(await page.locator('.pane').count(),0,'late resume cannot open in another Workspace');
 await panel.getByRole('button',{name:'Pin',exact:true}).focus();await page.keyboard.press('Escape');assert.ok(await panel.isHidden());
 await page.locator('#header-new summary').click();
 await page.getByLabel('Search projects').fill('project');
 await page.locator('.project-results button').waitFor();await page.keyboard.press('Enter');
 await page.waitForFunction(()=>document.querySelector('#gateway-status').textContent.includes('fixture'));
 assert.deepEqual(operations.find(op=>op.action==='create_workspace'),{action:'create_workspace',name:'project-2',cwd:'/project'});
 await page.getByRole('tab',{name:'Remotes',exact:true}).click();
 await page.getByRole('button',{name:'omarchy Connected',exact:true}).click();
 await page.getByRole('button',{name:'Rename connection…',exact:true}).click();
 await page.getByLabel('Display name').fill('office');await page.getByRole('button',{name:'Rename connection',exact:true}).click();
 await page.waitForFunction(()=>!document.querySelector('dialog[open]'));
 assert.deepEqual(operations.find(op=>op.action==='rename_node'),{action:'rename_node',id:'remote',name:'office',revision:7});
 await page.getByRole('button',{name:'Forget connection only…',exact:true}).click();await page.getByRole('button',{name:'Cancel',exact:true}).click();
 assert.equal(operations.filter(op=>op.action==='forget_node').length,0);
 await page.locator('#header-new summary').click();await page.locator('#header-connect').click();
 assert.ok(await page.getByRole('dialog').getByRole('button',{name:'omarchy · Connected',exact:true}).isVisible());await page.keyboard.press('Escape');
 assert.deepEqual(errors,[]);
 console.log('Conversations filtering/preferences, idempotent resume, workspace race, project search, remote rename and cancellation passed');
} finally {await browser.close();}
