import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[],gitRequests=[];
 page.on('pageerror',e=>errors.push(e.message));
 const shell={id:'shell',name:'build',cwd:'/project',status:'running',run:{id:'run'}};
 const info={node_id:'local',workspace_id:'workspace',nodes:[{id:'local',alias:'local',local:true},{id:'remote',alias:'omarchy',local:false,current:true,stale:false,health:'online',route:'ssh:omarchy'},{id:'offline',alias:'travel',local:false,current:false,stale:true,health:'authentication_required',route:'travel'}],snapshot:{workspaces:[{id:'workspace',name:'project',shells:[shell],agents:[{id:'agent',name:'Codex',integration:'codex',shell_id:'shell',run_id:'run',observation:{state:'working'}},{id:'old',name:'Old Agent',shell_id:'shell',run_id:'old-run',observation:{state:'working'}}]}]}};
 await page.route('**/api/snapshot',r=>r.fulfill({json:info}));
 await page.route('**/api/attach',r=>r.fulfill({status:409,json:{error:'could not attach build: shell already has an active controller; use takeover'}}));
 await page.route('**/api/git',r=>{gitRequests.push(r.request().postDataJSON());return r.fulfill({json:{worktrees:[{repository:'boomux',branch:'poc/webgpu-tiling',root:'/project',status:{staged:1,unstaged:2,untracked:0,conflicts:0},shells:[],pr:{}}],warnings:[]}});});
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4389');
 await page.waitForSelector('.workspace-button');
 await page.getByRole('button',{name:'Take control',exact:true}).waitFor();
 for(const hidden of [false,true,false]){
  await page.evaluate(hidden=>document.body.classList.toggle('hide-headings',hidden),hidden);
  const body=await page.locator('.pane-body').boundingBox(),overlay=await page.locator('.attachment-error').boundingBox();
  assert.deepEqual(overlay,body,'Take control covers the entire terminal with headings shown or hidden');
 }

 assert.equal(await page.locator('.attachment-error p').textContent(),'Another terminal controls this Shell. Take control to use it here.');
 const takeoverRequest=page.waitForRequest(request=>request.url().endsWith('/api/attach')&&request.postDataJSON()?.takeover===true);
 await page.getByRole('button',{name:'Take control',exact:true}).click();
 await takeoverRequest;
 assert.equal(await page.locator('.resource-dialog').count(),0,'Take control attaches immediately without confirmation');
 assert.ok((await page.locator('#stage').boundingBox()).y<15,'canvas starts at the top');
 assert.equal(await page.locator('header,.toolbar').count(),0);
 assert.equal(await page.locator('.brand-copy strong').textContent(),'BOOMUX');
 await page.locator('#header-new summary').click();assert.ok(await page.locator('#header-new-workspace').isVisible());await page.keyboard.press('Escape');
 await page.getByLabel('Settings',{exact:true}).click();
 const settingsBounds=await page.locator('.settings-menu').boundingBox();assert.ok(settingsBounds.x>=0&&settingsBounds.y>=0,'header settings stay on screen');assert.ok(await page.getByText('Layout & workspaces',{exact:true}).isVisible());
 await page.locator('[data-preference=motion][data-value=instant]').click();assert.equal(await page.locator('#motion').isChecked(),false);
 await page.getByRole('button',{name:'Close Settings',exact:true}).click();await page.keyboard.press('Control+Space');assert.equal(await page.locator('#layout-mode').getAttribute('aria-pressed'),'true');
 for(const viewport of [{width:2048,height:1230},{width:800,height:600},{width:480,height:700}]){
  await page.setViewportSize(viewport);
  const hint=await page.locator('#keyboard-help').boundingBox();
  assert.equal(hint.height,42,'Desktop layout badge height');
  assert.ok(hint.width<220,'layout badge stays compact');
  assert.ok(hint.x>=0&&hint.x+hint.width<=viewport.width,'layout hint fits horizontally');
  assert.ok(hint.y>=0&&hint.y<60,'layout badge stays at the top');
 }
 await page.setViewportSize({width:1440,height:950});
 await page.keyboard.press('Escape');
 await page.locator('#activity-content').click();assert.equal(await page.locator('#settings').getAttribute('open'),null);
 assert.ok(await page.getByRole('button',{name:/build.*working.*project.*codex/}).isVisible());assert.equal(await page.getByText('Old Agent',{exact:true}).count(),0);
 await page.getByRole('tab',{name:'Remotes',exact:true}).click();assert.ok(await page.getByText('omarchy',{exact:true}).isVisible());
 assert.equal(await page.getByRole('button',{name:'Update Boomux',exact:true}).count(),0);
 await page.getByRole('button',{name:'omarchy Connected',exact:true}).click();
 assert.ok(await page.getByRole('button',{name:'Update Boomux',exact:true}).isVisible());
 assert.equal(await page.getByRole('button',{name:'Sign in…',exact:true}).count(),1);
 await page.getByRole('button',{name:'travel Sign-in required',exact:true}).click();
 assert.equal(await page.getByRole('button',{name:'Update Boomux',exact:true}).count(),0);
 assert.ok(await page.getByRole('button',{name:'Sign in…',exact:true}).isVisible());
 await page.screenshot({path:'/tmp/boomux-header-remotes.png'});
 await page.getByRole('tab',{name:'Git',exact:true}).click();await page.getByText('boomux · poc/webgpu-tiling',{exact:true}).waitFor();
 await page.getByText('boomux · poc/webgpu-tiling',{exact:true}).click();assert.ok(await page.getByText('1 staged · 2 modified · 0 untracked',{exact:true}).isVisible());
 await page.getByLabel('Git Node',{exact:true}).selectOption('remote');await page.waitForTimeout(100);assert.equal(gitRequests.at(-1).owner,'remote');
 assert.equal(await page.locator('#activity-toggle,#activity-refresh,.sidebar-tools').count(),0);
 await page.getByLabel('Settings',{exact:true}).click();
 await page.locator('.settings-menu #theme-picker').click();
 await page.locator('#theme-dialog').waitFor();
 await page.keyboard.press('Escape');
 await page.waitForFunction(()=>document.activeElement.getAttribute('aria-label')==='Settings');
 await page.getByRole('tab',{name:'Agents',exact:true}).click();assert.ok(await page.locator('#activity-content').isVisible());
 const before=await page.locator('#activity').boundingBox();await page.getByRole('separator',{name:'Resize activity panel'}).focus();await page.keyboard.press('ArrowUp');assert.ok((await page.locator('#activity').boundingBox()).height>before.height);
 assert.deepEqual(errors,[]);await page.screenshot({path:'/tmp/boomux-panels.png'});console.log('Full-height canvas, settings, Agent filtering, remote health, Git routing, Settings theme picker, footer removal and resize passed');
}finally{await browser.close();}
