import {resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';

const root=dirname(fileURLToPath(import.meta.url));
const repo=resolve(root,'../..');
const port=Number(process.env.POC_PORT||4387);
const host=`127.0.0.1:${port}`,origin=`http://${host}`;
const sessions=new Set();
const maxSessions=48,maxPending=1024*1024;
const routes=new Map(['index.html','app.js','style.css','layout.js','renderer.js','terminal.js','themes.js'].map(name=>['/'+name,resolve(root,name)]));
routes.set('/',resolve(root,'index.html'));
routes.set('/vendor/ghostty-web.js',resolve(repo,'node_modules/ghostty-web/dist/ghostty-web.js'));
routes.set('/vendor/ghostty-vt.wasm',resolve(repo,'node_modules/ghostty-web/ghostty-vt.wasm'));
routes.set('/vendor/jetbrains-mono.woff2',resolve(repo,'node_modules/@fontsource/jetbrains-mono/files/jetbrains-mono-latin-400-normal.woff2'));
const dimension=(n,min,max)=>Number.isInteger(n)&&n>=min&&n<=max;
function cleanup(ws){
  const s=ws.data;if(s.closed)return;s.closed=true;sessions.delete(ws);
  clearTimeout(s.deadline);
  // Closing the PTY hangs up foreground work. The shell receives SIGHUP and
  // propagates it to ordinary jobs. Explicitly detached jobs are not supervised.
  s.proc?.terminal?.close();
  if(s.proc&&s.proc.exitCode===null){s.proc.kill('SIGHUP');const timer=setTimeout(()=>{if(s.proc.exitCode===null)s.proc.kill('SIGKILL');},500);s.proc.exited.finally(()=>clearTimeout(timer));}
}
function close(ws,reason){cleanup(ws);ws.close(1008,reason);}
const server=Bun.serve({hostname:'127.0.0.1',port,
  async fetch(req,server){
    if(req.headers.get('host')!==host)return new Response('Invalid host',{status:403});
    const url=new URL(req.url);
    if(url.pathname==='/pty'){
      if(req.headers.get('origin')!==origin)return new Response('Invalid origin',{status:403});
      if(sessions.size>=maxSessions)return new Response('Session limit',{status:429});
      const cols=Number(url.searchParams.get('cols')),rows=Number(url.searchParams.get('rows'));
      if(!dimension(cols,2,500)||!dimension(rows,1,200))return new Response('Invalid dimensions',{status:400});
      if(server.upgrade(req,{data:{cols,rows,pending:0,closed:false,inputBytes:0,inputWindow:Date.now()}}))return;
      return new Response('Expected WebSocket',{status:400});
    }
    if(req.method!=='GET')return new Response('Method not allowed',{status:405});
    const path=routes.get(url.pathname);if(!path)return new Response('Not found',{status:404});
    const file=Bun.file(path);if(!await file.exists())return new Response('Run bun install --frozen-lockfile from the repository root',{status:503});
    return new Response(file,{headers:{'Cache-Control':'no-store','X-Content-Type-Options':'nosniff','Cross-Origin-Resource-Policy':'same-origin','Content-Security-Policy':"default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; font-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'"}});
  },
  websocket:{maxPayloadLength:65536,backpressureLimit:maxPending,closeOnBackpressureLimit:true,idleTimeout:0,
    open(ws){
      if(sessions.size>=maxSessions){close(ws,'Session limit');return;}
      sessions.add(ws);const s=ws.data;
      try{
        // No startup files: the experiment must not inherit automatic agent
        // registration hooks from the parent or the user's interactive rc.
        const env={...process.env,TERM:'xterm-256color',COLORTERM:'truecolor',PS1:'\\w \\$ ',HISTFILE:'/dev/null'};
        for(const key of Object.keys(env))if(/^(BOOMUX_|OPENCODE_|CLAUDE_CODE_|CODEX_|BASH_FUNC_)/.test(key)||['BASH_ENV','ENV','PROMPT_COMMAND'].includes(key))delete env[key];
        s.proc=Bun.spawn(['bash','--noprofile','--norc','-i'],{cwd:repo,env,terminal:{cols:s.cols,rows:s.rows,
          data(_terminal,bytes){
            if(s.closed)return;
            if(s.pending+bytes.length>maxPending){close(ws,'Terminal output backlog');return;}
            s.pending+=bytes.length;
            if(ws.sendBinary(bytes)===0){close(ws,'Terminal transport closed');return;}
            if(!s.deadline)s.deadline=setTimeout(()=>close(ws,'Terminal output acknowledgement timeout'),15000);
          }
        }});
        ws.send(JSON.stringify({type:'ready',pid:s.proc.pid}));
        s.proc.exited.then(code=>{if(!s.closed){ws.send(JSON.stringify({type:'exit',code}));cleanup(ws);ws.close(1000,'Process exited');}});
      }catch{close(ws,'Could not start shell');}
    },
    message(ws,raw){
      const s=ws.data;if(s.closed)return;
      try{
        if(typeof raw!=='string')throw Error();
        const message=JSON.parse(raw);
        if(message.type==='ack'){
          if(!Number.isInteger(message.bytes)||message.bytes<1||message.bytes>s.pending)throw Error();
          s.pending-=message.bytes;clearTimeout(s.deadline);s.deadline=s.pending?setTimeout(()=>close(ws,'Terminal output acknowledgement timeout'),15000):null;return;
        }
        if(message.type==='resize'){
          if(!dimension(message.cols,2,500)||!dimension(message.rows,1,200))throw Error();
          s.proc.terminal.resize(message.cols,message.rows);return;
        }
        if(message.type==='input'&&typeof message.data==='string'){
          if(Date.now()-s.inputWindow>1000){s.inputWindow=Date.now();s.inputBytes=0;}
          s.inputBytes+=Buffer.byteLength(message.data);if(s.inputBytes>65536)throw Error();
          s.proc.terminal.write(message.data);return;
        }
        throw Error();
      }catch{close(ws,'Invalid or excessive terminal input');}
    },
    close(ws){cleanup(ws);}
  }
});
function shutdown(){for(const ws of sessions)close(ws,'Server stopped');server.stop(true);}
process.on('SIGINT',shutdown);process.on('SIGTERM',shutdown);
console.log(`Ghostty tiling lab: ${origin}`);
