// Exercises the shipped, patched Ghostty bundle with real browser key/wheel events.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage();
 await page.route('**/input-history-fixture',route=>route.fulfill({contentType:'text/html',body:'<div id="terminal" style="width:640px;height:320px"></div>'}));
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4387')+'/input-history-fixture');
 await page.evaluate(async()=>{
  const {Ghostty,Terminal}=await import('/vendor/ghostty-web.js');
  const module=await WebAssembly.compile(await(await fetch('/vendor/ghostty-vt.wasm')).arrayBuffer());
  const instance=await WebAssembly.instantiate(module,{env:{log(){}}});
  const term=window.term=new Terminal({ghostty:new Ghostty(instance),cols:60,rows:12,scrollback:200,smoothScrollDuration:100});
  term.open(document.querySelector('#terminal'));window.sent=[];term.onData(data=>window.sent.push(data));
  window.topLine=()=>String.fromCodePoint(...term.getScrollbackLine(Math.floor(term.getScrollbackLength()-term.viewportY)).map(cell=>cell.codepoint||32));
 });
 await page.keyboard.press('Shift+Enter');
 await page.keyboard.press('Enter');
 assert.deepEqual(await page.evaluate(()=>sent),['\n','\r'],'Shift+Enter inserts a newline; Enter still submits');
 await page.evaluate(()=>{term.write(Array.from({length:70},(_,i)=>`line ${i}\r\n`).join(''));term.scrollLines(-25);});
 const top=await page.evaluate(()=>topLine());
 await page.evaluate(()=>{for(let i=0;i<10;i++)term.write(`incoming ${i}\r\n`);});
 assert.equal(await page.evaluate(()=>topLine()),top,'incoming output preserves the visible history line');
 await page.evaluate(()=>term.scrollToBottom());
 await page.mouse.move(150,80);await page.mouse.wheel(0,-140);
 await page.waitForFunction(()=>term.scrollAnimationFrame!==undefined);
 const target=await page.evaluate(()=>term.getScrollbackLength()-term.targetViewportY);
 await page.evaluate(()=>term.write('during wheel\r\n'));
 await page.waitForFunction(()=>term.scrollAnimationFrame===undefined);
 assert.ok(Math.abs(await page.evaluate(()=>term.getScrollbackLength()-term.viewportY)-target)<.01,'output preserves the wheel animation destination');
 await page.evaluate(()=>{term.scrollToBottom();term.write('latest\r\n');});
 assert.equal(await page.evaluate(()=>term.viewportY),0,'tailing output still follows new lines');
 await page.evaluate(()=>{term.scrollLines(-20);term.write('overflow\r\n'.repeat(400));});
 assert.ok(await page.evaluate(()=>term.viewportY>0&&term.viewportY<=term.getScrollbackLength()),'history eviction remains bounded without snapping to live output');
 await page.evaluate(()=>term.dispose());
 console.log('Shift+Enter, live-output history anchoring, wheel animation, tail following, and bounded history passed');
}finally{await browser.close();}
