import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1200,height:800}}),errors=[];
 page.on('pageerror',e=>errors.push(e.message));
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'test',workspace_id:'w',nodes:[],snapshot:{workspaces:[{id:'w',name:'Workspace',agents:[],shells:[]}]}}}));
 await page.route('**/api/changes',r=>r.fulfill({status:404}));
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4390');await page.waitForSelector('.workspace-button');
 const handle=page.getByRole('separator',{name:'Resize sidebar',exact:true});
 const hidden=()=>page.locator('body').evaluate(el=>el.classList.contains('sidebar-hidden'));
 async function start(){const r=await handle.boundingBox();await page.mouse.move(r.x+r.width/2,200);await page.mouse.down();}
 await start();await page.mouse.move(180,200);await page.mouse.up();
 assert.equal(await page.locator('aside').evaluate(el=>el.offsetWidth),180);
 assert.equal(await page.locator('.brand-copy').isVisible(),false,'compact header retains controls without overlapping the title');
 const brand=await page.locator('.brand-mark').boundingBox(),actions=await page.locator('.sidebar-header-actions').boundingBox();
 assert.ok(brand.x+brand.width<actions.x&&actions.x+actions.width<=180);
 await start();await page.mouse.move(25,200);assert.ok(await hidden());
 await page.mouse.move(300,200);assert.equal(await hidden(),false,'same captured gesture can reopen');await page.mouse.up();
 assert.equal(await page.locator('aside').evaluate(el=>el.offsetWidth),300);
 assert.ok(await page.locator('.brand-copy').isVisible());
 await start();await page.mouse.move(20,200);await page.mouse.up();assert.ok(await hidden());
 await start();await page.mouse.move(330,200);await page.mouse.up();assert.equal(await hidden(),false,'drag from left edge reopens sidebar');
 await start();await page.mouse.move(20,200);await page.keyboard.press('Escape');await page.mouse.up();
 assert.equal(await hidden(),false);assert.equal(await page.locator('aside').evaluate(el=>el.offsetWidth),330,'cancel restores width and visibility');
 assert.deepEqual(errors,[]);console.log('Compact header, drag collapse/reopen, left-edge restore, and Escape cancellation passed');
}finally{await browser.close();}
