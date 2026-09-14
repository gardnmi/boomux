const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
import assert from 'node:assert/strict';
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
 const page=await browser.newPage({viewport:{width:877,height:1120},deviceScaleFactor:1.25});
 const errors=[];page.on('pageerror',e=>errors.push(e.message));
 await page.addInitScript(()=>{localStorage.setItem('boomux.webgpu.theme','kanagawa');localStorage.setItem('boomux.web.preferences',JSON.stringify({sidebarWidth:336,motion:'smooth'}));});
 const names=Array.from({length:18},(_,i)=>'Workspace '+i);
 const workspaces=names.map((name,i)=>({id:`w${i}`,name,shells:(i===0?['gentle-whale','fair-jay']:['shell']).map((name,j)=>({id:`s${i}${j}`,workspace_id:`w${i}`,name,cwd:'/project',status:'running',run:{id:`r${i}${j}`}})),agents:[]}));
 await page.route('**/api/**',r=>r.fulfill({status:r.request().url().includes('/attach')?409:200,json:r.request().url().includes('/snapshot')?{node_id:'local',workspace_id:'w0',nodes:[{id:'local',local:true},{id:'remote',local:false,alias:'omarchy',health:'online',current:true,stale:false}],snapshot:{workspaces}}:{error:'shell already has an active controller; use takeover'}}));

 await page.goto((process.env.POC_URL||'http://127.0.0.1:4390')+'/?fallback');await page.locator('.workspace-heading').first().waitFor();
 for(const index of [0,17]){
  const menu=page.locator('.workspace-heading .sidebar-menu').nth(index);
  await menu.locator('summary').click();const popup=menu.locator('.sidebar-menu-items');await popup.waitFor({state:'visible'});
  const bounds=await popup.boundingBox();assert.ok(bounds.y>=0&&bounds.y+bounds.height<=1120);
  assert.ok(await popup.evaluate(el=>{const b=el.getBoundingClientRect();return el.contains(document.elementFromPoint(b.x+b.width/2,b.y+b.height-10));}),'menu is above rows and scroll boundaries');

  await page.keyboard.press('Escape');await page.waitForFunction(()=>!document.querySelector('.sidebar-menu[open]'));
 }
 assert.deepEqual(errors,[]);console.log('Workspace menus escape row clipping and remain visible at list bottom');
}finally{await browser.close();}
