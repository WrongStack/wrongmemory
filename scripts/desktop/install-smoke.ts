#!/usr/bin/env bun
// Public installer -> private mirror -> disposable root/prefix. Never launches GUI.
import {existsSync,mkdirSync,readFileSync,writeFileSync} from 'node:fs';
import {join,resolve,basename} from 'node:path';
import {randomUUID,createHash} from 'node:crypto';
const repo=resolve(import.meta.dir,'../..');
const args=process.argv.slice(2);
const option=(name:string)=>{const i=args.indexOf(name);if(i<0||!args[i+1])throw Error('Missing '+name);return resolve(args[i+1]!);};
const output=option('--out'); const bundle=option('--bundle');
const scratch=join(repo,'target','desktop-install-smoke-'+randomUUID());mkdirSync(scratch,{recursive:true});
const home=join(scratch,'data'); const prefix=join(scratch,'installed with spaces');
const cli=join(bundle,'bin','wrongmemory.exe');
const env={...process.env,WRONGMEMORY_HOME:home,WRONGMEMORY_INSTALL_DIR:prefix,WRONGMEMORY_DESKTOP:'1',WRONGMEMORY_NO_MODIFY_PATH:'1',WRONGMEMORY_VERIFY_ATTESTATION:'0',WRONGMEMORY_AUTOSTART:'0',WRONGMEMORY_AUTOSTART_KEY_OVERRIDE:'HKCU\\Software\\WrongMemoryTest\\Desktop-'+randomUUID()};
async function run(command:string[],extra:Record<string,string>={}):Promise<{code:number;text:string}> {
  const process=Bun.spawn(command,{env:{...env,...extra},stdout:'pipe',stderr:'pipe'});
  const timer=setTimeout(()=>process.kill(),60000);
  try {const [code,out,err]=await Promise.all([process.exited,new Response(process.stdout).text(),new Response(process.stderr).text()]);return {code,text:out+'\n'+err};}finally{clearTimeout(timer);}
}
async function ok(command:string[]):Promise<string>{const r=await run(command);if(r.code)throw Error(r.text);return r.text;}
const assert=(condition:unknown,message:string)=>{if(!condition)throw Error(message);};
const sums=readFileSync(join(output,'SHA256SUMS'),'utf8');
const zip=sums.split(/\r?\n/).map(l=>l.split('  ')[1]).find(n=>n?.endsWith('.zip'))!;
const server=Bun.serve({hostname:'127.0.0.1',port:0,fetch(request){
  const u=new URL(request.url);const name=basename(u.pathname);
  if(name==='SHA256SUMS')return new Response(u.pathname.startsWith('/corrupt/')?sums.replace(/^[a-f0-9]{64}/,'0'.repeat(64)):sums);
  if(name===zip)return new Response(Bun.file(join(output,zip)));
  return new Response('Not found',{status:404});
}});
const installer=['powershell','-NoProfile','-ExecutionPolicy','Bypass','-File',join(repo,'install.ps1')];
try {
  const base='http://127.0.0.1:'+server.port;
  const rejected=await run(installer,{WRONGMEMORY_DOWNLOAD_BASE:base+'/corrupt'});
  writeFileSync(join(scratch,'rejected-download.txt'),rejected.text);
  assert(rejected.code!==0&&rejected.text.includes('checksum mismatch'),'Corrupt download must fail: '+rejected.text);
  assert(!existsSync(prefix),'Rejected archive changed install prefix');
  const installed=await run(installer,{WRONGMEMORY_DOWNLOAD_BASE:base});assert(installed.code===0,installed.text);
  const installedCli=join(prefix,'wrongmemory.exe');const gui=join(prefix,'wrongmemory-desktop.exe');
  assert(existsSync(gui)&&existsSync(join(prefix,'desktop-resources','LICENSE')),'Desktop/resources missing');
  assert(!existsSync(join(home,'registry.db')),'Fresh install unexpectedly initialized a database');
  const project=join(scratch,'project');mkdirSync(project);
  const registered=JSON.parse(await ok([installedCli,'--json','project','add',project,'--managed']));
  const id=registered.project.projectId;const store=registered.project.storePath;
  await ok([installedCli,'remember',id,'InstallerSmoke preserved knowledge after update']);
  await ok([installedCli,'autostart','enable','--prefix',prefix]);
  const updated=await run(installer,{WRONGMEMORY_DOWNLOAD_BASE:base});assert(updated.code===0,updated.text);
  assert(existsSync(gui+'.old'),'Previous GUI was not retained');
  const matches=await ok([installedCli,'--json','search',id,'InstallerSmoke']);assert(matches.includes('preserved knowledge'),'Update lost knowledge');
  const sha=(path:string)=>createHash('sha256').update(readFileSync(path)).digest('hex');
  assert(sha(gui)===sha(join(bundle,'bin','wrongmemory-desktop.exe')),'Installed GUI differs from bundle');
  await ok([cli,'uninstall','--prefix',prefix]);
  for(const name of ['wrongmemory.exe','wrongmemoryd.exe','wrongmemory-desktop.exe'])assert(!existsSync(join(prefix,name)),'Uninstall kept '+name);
  assert(!existsSync(join(prefix,'desktop-resources')),'Owned resources were not removed');
  assert(existsSync(store)&&existsSync(join(home,'registry.db')),'Uninstall removed knowledge');
  const auto=await ok([cli,'--json','autostart','status']);assert(JSON.parse(auto).command===null,'Uninstall kept autostart');
  const report={result:'PASS',scratch,checks:['corrupt download refused before changes','fresh desktop install without store initialization','update with live disposable daemon','knowledge and GUI backup retained','installed GUI byte identity','uninstall and autostart cleanup','knowledge preserved by default']};
  writeFileSync(join(scratch,'RESULT.json'),JSON.stringify(report,null,2));console.log(JSON.stringify(report,null,2));
} finally {
  await run([cli,'daemon','stop']);
  // Only the uniquely named scratch registry key, never its parent or real Run key.
  await run(['reg','delete',env.WRONGMEMORY_AUTOSTART_KEY_OVERRIDE,'/f']);server.stop(true);
}
