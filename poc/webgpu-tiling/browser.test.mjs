// Run against the local server with an existing Playwright installation:
// PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs node browser.test.mjs
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM || '/usr/bin/chromium',headless:process.env.HEADED!=='1',args:['--no-sandbox','--enable-unsafe-webgpu']});
try {
  for(const suffix of ['', '?fallback']){
    const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[];
    page.on('pageerror',e=>errors.push(e.message));
    await page.goto(`http://127.0.0.1:4387/${suffix}`);
    await page.waitForFunction(()=>!document.querySelector('#renderer').textContent.includes('Starting'));
    await page.waitForTimeout(300);
    if(process.env.REQUIRE_WEBGPU==='1'&&!suffix)assert.match(await page.locator('#renderer').textContent(),/WebGPU · pane compositor/);
    const pane=id=>page.locator(`.pane[data-id="${id}"]`);
    const box=id=>pane(id).boundingBox();
    const settle=()=>page.waitForTimeout(220);
    const start=async id=>{const r=await box(id);await page.mouse.move(r.x+85,r.y+20);await page.mouse.down();return r;};
    const initial=await box(1);
    await start(1);await page.mouse.move(initial.x+130,initial.y+100,{steps:5});await settle();
    await page.keyboard.press('Escape');await page.mouse.up();await settle();
    assert.deepEqual(await box(1),initial,'cancel must restore exact layout');
    const lifted=await start(1);await page.mouse.move(lifted.x+105,lifted.y+35,{steps:3});await settle();let target=await box(4);await page.mouse.move(target.x+target.width-15,target.y+target.height/2,{steps:10});await settle();
    assert.equal(await page.locator('#drop-label').textContent(),'Tile right');
    await page.mouse.up();await settle();
    assert.ok((await box(1)).x>(await box(4)).x,'pane should tile to the right');
    await start(1);await page.keyboard.down('Shift');await page.mouse.move(850,280,{steps:8});await page.mouse.up();await page.keyboard.up('Shift');await settle();
    assert.equal(await page.locator('.sidebar-pane.selected small').textContent(),'float');
    await pane(1).getByRole('button',{name:'Toggle floating',exact:true}).click();await settle();
    assert.notEqual(await page.locator('.sidebar-pane.selected small').textContent(),'float');
    await pane(1).locator('.pane-heading').dblclick({position:{x:60,y:20}});await settle();
    assert.equal(await page.locator('.pane:visible').count(),1);
    await page.keyboard.press('Escape');await settle();assert.equal(await page.locator('.pane:visible').count(),4);
    await page.locator('#reset').click();await settle();
    const before=await box(1),d=await page.locator('.divider.x').first().boundingBox();
    await page.mouse.move(d.x+4,d.y+70);await page.mouse.down();await page.mouse.move(d.x-100,d.y+70,{steps:8});await page.mouse.up();await settle();
    assert.ok((await box(1)).width<before.width-50,'divider should resize columns');
    await page.locator('#add').click();await settle();assert.equal(await page.locator('.pane').count(),5);
    await pane(5).getByRole('button',{name:'Remove demo pane',exact:true}).click();assert.equal(await page.locator('.pane').count(),4);
    await page.setViewportSize({width:1100,height:750});await settle();
    for(const el of await page.locator('.pane').all()){const r=await el.boundingBox();assert.ok(r.x>=0&&r.x+r.width<=1100&&r.y+r.height<=750);}
    assert.deepEqual(errors,[]);
    console.log(`${suffix||'default'}: ${await page.locator('#renderer').textContent()}; drag, cancel, float, expand, resize, add/remove, viewport passed`);
    await page.close();
  }
} finally {await browser.close();}
