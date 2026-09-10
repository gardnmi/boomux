import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[],gitRequests=[];
 page.on('pageerror',e=>errors.push(e.message));
 const shell={id:'shell',name:'build',cwd:'/project',status:'running',run:{id:'run'}};
 const info={node_id:'local',workspace_id:'workspace',nodes:[{id:'local',alias:'local',local:true},{id:'remote',alias:'omarchy',local:false,current:true,stale:false,health:'online',route:'ssh:omarchy'}],snapshot:{workspaces:[{id:'workspace',name:'project',shells:[shell],agents:[{id:'agent',name:'Codex',shell_id:'shell',run_id:'run',observation:{state:'working'}},{id:'old',name:'Old Agent',shell_id:'shell',run_id:'old-run',observation:{state:'working'}}]}]}};
 await page.route('**/api/snapshot',r=>r.fulfill({json:info}));
 await page.route('**/api/attach',r=>r.fulfill({status:409,json:{error:'Preview only'}}));
 await page.route('**/api/git',r=>{gitRequests.push(r.request().postDataJSON());return r.fulfill({json:{worktrees:[{repository:'boomux',branch:'poc/webgpu-tiling',root:'/project',status:{staged:1,unstaged:2,untracked:0,conflicts:0},shells:[],pr:{}}],warnings:[]}});});
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4389');
 await page.waitForSelector('.workspace-button');
 assert.ok((await page.locator('#stage').boundingBox()).y<15,'canvas starts at the top');
 assert.equal(await page.locator('header,.toolbar').count(),0);
 await page.getByLabel('Settings',{exact:true}).click();assert.ok(await page.getByText('Animate layout',{exact:true}).isVisible());
 await page.locator('#motion').uncheck();assert.equal(await page.locator('#motion').isChecked(),false);
 await page.locator('#layout-mode').click();assert.equal(await page.locator('#layout-mode').getAttribute('aria-pressed'),'true');await page.keyboard.press('Escape');
 await page.locator('#activity-content').click();assert.equal(await page.locator('#settings').getAttribute('open'),null);
 assert.ok(await page.getByRole('button',{name:/Codex.*project/}).isVisible());assert.equal(await page.getByText('Old Agent',{exact:true}).count(),0);
 await page.getByRole('tab',{name:'Remotes',exact:true}).click();assert.ok(await page.getByText('omarchy',{exact:true}).isVisible());
 await page.getByRole('tab',{name:'Git',exact:true}).click();await page.getByText('boomux · poc/webgpu-tiling',{exact:true}).waitFor();
 await page.getByText('boomux · poc/webgpu-tiling',{exact:true}).click();assert.ok(await page.getByText('1 staged · 2 modified · 0 untracked',{exact:true}).isVisible());
 await page.getByLabel('Git Node',{exact:true}).selectOption('remote');await page.waitForTimeout(100);assert.equal(gitRequests.at(-1).owner,'remote');
 await page.getByRole('button',{name:'Collapse activity',exact:true}).click();assert.equal(await page.locator('#activity-content').isVisible(),false);
 await page.getByRole('tab',{name:'Agents',exact:true}).click();assert.ok(await page.locator('#activity-content').isVisible());
 const before=await page.locator('#activity').boundingBox();await page.getByRole('separator',{name:'Resize activity panel'}).focus();await page.keyboard.press('ArrowUp');assert.ok((await page.locator('#activity').boundingBox()).height>before.height);
 assert.deepEqual(errors,[]);await page.screenshot({path:'/tmp/boomux-panels.png'});console.log('Full-height canvas, settings, Agent filtering, remote health, Git routing, collapse and resize passed');
}finally{await browser.close();}
