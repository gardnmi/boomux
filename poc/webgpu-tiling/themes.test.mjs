import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const b=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{const p=await b.newPage({viewport:{width:1440,height:950}}),errors=[];let connections=0;
p.on('pageerror',e=>errors.push(e.message));await p.route('**/api/snapshot',r=>r.fulfill({status:404,body:''}));await p.routeWebSocket('**/pty?*',ws=>{connections++;ws.send(JSON.stringify({type:'ready',pid:123}));ws.send(Buffer.from('persistent output\r\n\x1b[31mred\x1b[0m $ \r\n\x1b[38;2;18;58;188mtruecolor\x1b[0m'));});
await p.goto(process.env.POC_URL||'http://127.0.0.1:4389');await p.waitForSelector('.pane-body[data-connected="true"]');await p.waitForTimeout(300);
const initial=connections;
async function hasInk(rgb){
 return p.locator('.pane-body canvas').first().evaluate((canvas,rgb)=>{
  const pixels=canvas.getContext('2d').getImageData(0,0,canvas.width,56).data;
  for(let i=0;i<pixels.length;i+=4)if(pixels[i]===rgb[0]&&pixels[i+1]===rgb[1]&&pixels[i+2]===rgb[2]&&pixels[i+3]===255)return true;
  return false;
 },rgb);
}
await p.getByRole('button',{name:'Choose theme',exact:true}).click();assert.equal(await p.locator('.theme-choice').count(),23);
await p.getByRole('button',{name:'Catppuccin Latte',exact:true}).click();assert.equal(await p.locator('html').getAttribute('data-theme'),'boomux','preview does not apply');
await p.getByRole('button',{name:'Apply theme',exact:true}).click();await p.waitForFunction(()=>!document.documentElement.classList.contains('theme-wiping'));await p.waitForTimeout(100);
assert.equal(await p.locator('html').getAttribute('data-theme'),'catppuccin-latte');assert.equal(connections,initial,'theme does not recreate attachments');
const bg=await p.locator('.pane-body').first().evaluate(el=>getComputedStyle(el).backgroundColor);assert.equal(bg,'rgb(239, 241, 245)');
const pixel=await p.locator('.pane-body canvas').first().evaluate(c=>Array.from(c.getContext('2d').getImageData(100,100,1,1).data));assert.deepEqual(pixel.slice(0,3),[239,241,245],'existing terminal canvas receives light background');
assert.ok(await hasInk([76,79,105]),'existing foreground text recolors');
assert.ok(await hasInk([210,15,57]),'existing ANSI red recolors');
assert.ok(await hasInk([18,58,188]),'unrelated truecolor survives');
await p.getByRole('button',{name:'Choose theme',exact:true}).click();await p.getByRole('button',{name:'Tokyo Night',exact:true}).click();await p.keyboard.press('Escape');assert.equal(await p.locator('html').getAttribute('data-theme'),'catppuccin-latte');
await p.reload();await p.waitForSelector('.pane-body[data-connected="true"]');assert.equal(await p.locator('html').getAttribute('data-theme'),'catppuccin-latte','theme persists');
await p.waitForFunction(()=>!document.documentElement.classList.contains('theme-wiping'));await p.waitForTimeout(100);assert.ok(await hasInk([210,15,57]),'new terminal starts with saved ANSI palette');
await p.getByRole('button',{name:'Choose theme',exact:true}).click();await p.getByRole('button',{name:'Tokyo Night',exact:true}).click();await p.getByRole('button',{name:'Apply theme',exact:true}).click();await p.waitForFunction(()=>!document.documentElement.classList.contains('theme-wiping'));await p.waitForTimeout(100);
assert.ok(await hasInk([247,118,142]),'second theme maps from terminal startup palette');
await p.getByRole('button',{name:'Choose theme',exact:true}).click();await p.screenshot({path:'/tmp/boomux-theme-picker.png'});assert.deepEqual(errors,[]);console.log('23 palettes, preview/cancel, light canvas recoloring, retained connections, and persistence passed');
}finally{await b.close();}
