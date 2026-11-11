#!/usr/bin/env bun
// Resolve omitted workspace license texts at the exact crate VCS commit.
import {mkdirSync,readFileSync,readdirSync,existsSync,writeFileSync} from 'node:fs';
import {dirname,join,resolve,posix} from 'node:path';
import {createHash} from 'node:crypto';
const repo=resolve(import.meta.dir,'../..');
const result=Bun.spawnSync(['cargo','metadata','--format-version','1','--locked','--filter-platform','x86_64-pc-windows-msvc'],{cwd:join(repo,'apps/desktop'),stdout:'pipe',stderr:'inherit'});
if(result.exitCode)throw Error('Cargo metadata failed');
const metadata=JSON.parse(result.stdout.toString());
const target=join(repo,'apps/desktop/third-party-licenses');mkdirSync(target,{recursive:true});
const cache=new Map<string,Promise<string|null>>();
function get(url:string):Promise<string|null>{if(!cache.has(url))cache.set(url,(async()=>{try{const r=await fetch(url,{signal:AbortSignal.timeout(7000)});if(!r.ok)return null;const text=await r.text();return text.length>100&&!text.includes('<html')?text:null;}catch{return null;}})());return cache.get(url)!;}
const lock:any=Bun.TOML.parse(readFileSync(join(repo,'apps/desktop/Cargo.lock'),'utf8'));
const missing=metadata.packages.filter((p:any)=>p.source?.startsWith('registry+')&&(p.license==='MPL-2.0'||!readdirSync(dirname(p.manifest_path)).some(f=>/^(licen[cs]e|copying|notice)([.-]|$)/i.test(f))));
const entries:any[]=[];
for(const p of missing){
 const vcsPath=join(dirname(p.manifest_path),'.cargo_vcs_info.json');const vcs=existsSync(vcsPath)?JSON.parse(readFileSync(vcsPath,'utf8')):null;
 const sha=vcs?.git?.sha1;let url=p.repository as string|undefined;
 if(!url&&p.name.startsWith('zune-'))url='https://github.com/etemesi254/zune-image';
 const gh=url?.match(/^https:\/\/github.com\/([^/]+\/[^/]+)/)?.[1]?.replace(/\.git$/,'');
 const gl=url?.startsWith('https://gitlab.redox-os.org/')?url.replace(/\.git$/,''):null;
 const refs=sha?[sha]:['v'+p.version,p.version];let found:any[]=[];
 for(const ref of refs){
  const dirs:string[]=[];let dir=vcs?.path_in_vcs??'';while(true){dirs.push(dir);if(!dir)break;dir=posix.dirname(dir);if(dir==='.')dir='';}
  for(const path of dirs){
   const names=['LICENSE','LICENSE-MIT','LICENSE-APACHE','LICENSE.md','LICENSE.txt','COPYING','license','license.md'];
   const candidates=names.map(name=>({name,url:gh?'https://raw.githubusercontent.com/'+gh+'/'+ref+'/'+(path?path+'/':'')+name:gl?gl+'/-/raw/'+ref+'/'+name:''})).filter(x=>x.url);
   const responses=await Promise.all(candidates.map(async c=>({...c,text:await get(c.url)})));
   for(const r of responses.filter(r=>r.text)){
    const file=p.name+'-'+p.version+'-'+r.name;writeFileSync(join(target,file),r.text!);
    found.push({file,url:r.url,sha256:createHash('sha256').update(r.text!).digest('hex')});
   }
   if(found.length)break;
  }
  if(found.length)break;
 }
 let declarationOnly=false;
 if(!found.length){
  declarationOnly=true;
  const original=join(dirname(p.manifest_path),'Cargo.toml.orig');
  const declaration=readFileSync(existsSync(original)?original:p.manifest_path,'utf8');
  const file=p.name+'-'+p.version+'-DECLARATION.toml';writeFileSync(join(target,file),declaration);
  found.push({file,url:'https://crates.io/api/v1/crates/'+p.name+'/'+p.version+'/download',sha256:createHash('sha256').update(declaration).digest('hex'),kind:'upstream-package-declaration'});
  const license=p.license.includes('Apache-2.0')?'Apache-2.0':'MIT';
  const url='https://raw.githubusercontent.com/spdx/license-list-data/main/text/'+license+'.txt';const text=await get(url);if(!text)throw Error('Missing standard license '+license);
  const licenseFile=p.name+'-'+p.version+'-STANDARD-'+license+'.txt';writeFileSync(join(target,licenseFile),text);
  found.push({file:licenseFile,url,sha256:createHash('sha256').update(text).digest('hex'),kind:'standard-license-text'});
 }
 if(declarationOnly||p.license==='MPL-2.0'){
  const url='https://crates.io/api/v1/crates/'+p.name+'/'+p.version+'/download';const response=await fetch(url,{signal:AbortSignal.timeout(30000)});if(!response.ok)throw Error('Source archive download failed');
  const bytes=Buffer.from(await response.arrayBuffer());const hash=createHash('sha256').update(bytes).digest('hex');
  const expected=lock.package.find((v:any)=>v.name===p.name&&v.version===p.version)?.checksum;if(hash!==expected)throw Error('Crate source checksum mismatch');
  const file=p.name+'-'+p.version+'.crate';writeFileSync(join(target,file),bytes);found.push({file,url,sha256:hash,kind:'corresponding-source'});
 }
 entries.push({name:p.name,version:p.version,license:p.license,vcs:sha??null,declarationOnly,files:found});console.log(p.name+' '+found.length+' license/source attachments');
}
writeFileSync(join(target,'SOURCES.json'),JSON.stringify({schema:'wrongmemory/upstream-license-texts@1',packages:entries},null,2)+'\n');
if(entries.some(p=>!p.files.length)){console.error('Unresolved:',entries.filter(p=>!p.files.length).map(p=>p.name));process.exitCode=1;}
