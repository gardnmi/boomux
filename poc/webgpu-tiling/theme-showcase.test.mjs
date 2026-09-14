import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[];
 page.on('pageerror',error=>errors.push(error.message));
 await page.addInitScript(()=>{
  localStorage.setItem('boomux.webgpu.theme-view','list');
  localStorage.setItem('boomux.webgpu.theme','kanagawa');
 });
 await page.route('**/api/**',route=>route.fulfill({json:route.request().url().includes('/snapshot')?{node_id:'fixture',workspace_id:'w',nodes:[],snapshot:{workspaces:[{id:'w',name:'Example',shells:[],agents:[]}]}}:{}}));
 await page.goto(process.env.POC_URL||'http://127.0.0.1:4390');
 await page.keyboard.press('Control+Shift+Y');
 const dialog=page.locator('#theme-dialog');await dialog.waitFor();
 assert.equal(await page.locator('#theme-view,.theme-list').count(),0);
 assert.equal(await page.locator('.showcase-card').count(),3);
 assert.equal(await page.locator('#showcase-name').textContent(),'Kanagawa');
 const original=await page.locator('html').getAttribute('data-theme');
 await page.keyboard.press('ArrowRight');
 const candidate=await page.locator('#showcase-name').textContent();assert.notEqual(candidate,'Kanagawa');
 assert.equal(await page.locator('html').getAttribute('data-theme'),original,'browsing only previews');
 await page.keyboard.press('Enter');await dialog.waitFor({state:'hidden'});
 assert.notEqual(await page.locator('html').getAttribute('data-theme'),original);
 await page.waitForFunction(()=>!document.documentElement.classList.contains('theme-wiping'));
 await page.keyboard.press('Control+Shift+Y');await dialog.waitFor();
 assert.equal(await page.locator('#showcase-name').textContent(),candidate);
 await page.keyboard.press('ArrowRight');await page.keyboard.press('Escape');
 await page.keyboard.press('Control+Shift+Y');await dialog.waitFor();
 assert.equal(await page.locator('#showcase-name').textContent(),candidate,'Escape discards preview');
 await page.waitForTimeout(300);
 await page.screenshot({path:'/tmp/boomux-theme-showcase-desktop.png'});
 await page.setViewportSize({width:390,height:844});
 const bounds=await dialog.boundingBox();assert.ok(bounds.x>=0&&bounds.x+bounds.width<=390);
 assert.ok(await page.locator('#theme-apply').isVisible());
 assert.equal(await page.locator('.showcase-card').first().evaluate(el=>getComputedStyle(el,'::after').content),'none','hover cannot tint palette previews');
 await page.screenshot({path:'/tmp/boomux-theme-showcase-mobile.png'});
 assert.deepEqual(errors,[]);
 console.log('Showcase-only picker: ignores legacy view, arrows preview, Enter applies, Escape cancels, narrow viewport fits');
}finally{await browser.close();}
