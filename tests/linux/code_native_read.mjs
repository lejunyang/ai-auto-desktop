import fs from 'node:fs';
const request=JSON.parse(fs.readFileSync(0,'utf8'));
if(!new Set(['Runtime.evaluate','Browser.getVersion','Page.captureScreenshot']).has(request.method)) {
  throw new Error('this diagnostic client only supports read-only test methods');
}
const ws=new WebSocket(process.argv[2]);
const timer=setTimeout(()=>{console.error('CDP read timed out');process.exit(1);},15000);
ws.addEventListener('open',()=>ws.send(JSON.stringify(request)));
ws.addEventListener('error',()=>{console.error('CDP connection failed');process.exit(1);});
ws.addEventListener('message',event=>{
  const reply=JSON.parse(event.data);
  if(reply.id===request.id){clearTimeout(timer);process.stdout.write(JSON.stringify(reply),()=>{ws.close();process.exit(0);});}
});
