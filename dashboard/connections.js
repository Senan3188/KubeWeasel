'use strict';
const connectionTypes = [['postgresql','PostgreSQL'],['mysql','MySQL'],['redis','Redis'],['kafka','Kafka'],['custom','Custom / secrets']];
function connectionDialog(title) {
 const dialog=node('dialog','cluster-dialog connection-dialog'),body=node('div','connection-body'),head=node('div','dialog-heading');
 const close=node('button','close','×');close.type='button';close.setAttribute('aria-label','Close');close.addEventListener('click',()=>dialog.close());
 head.append(node('h2','',title),close);dialog.append(head,body);document.body.append(dialog);
 dialog.addEventListener('close',()=>{for(const input of dialog.querySelectorAll('input'))input.value='';dialog.remove();},{once:true});dialog.showModal();return {dialog,body};
}
function connectionError() {const error=node('p','form-error');error.setAttribute('role','alert');return error;}
function connectionTypeLabel(kind){return connectionTypes.find(t=>t[0]===kind)?.[1]||kind;}
function connectionEditor(project,cluster,connection,onSaved) {
 const {dialog,body}=connectionDialog(connection?'Edit connection':'New connection');
 const form=node('form','settings-form'),name=field('Connection name','name',connection?.name||'','text',{required:'',maxlength:'80',placeholder:'Postgres, MySQL, Analytics Postgres…'});
 const typeLabel=node('label','setting-field','Connection type'),type=node('select','project-select');
 for(const [value,label] of connectionTypes){const option=node('option','',label);option.value=value;type.append(option);}type.value=connection?.kind||'postgresql';typeLabel.append(type);
 const rows=node('div','connection-values'),removed=new Set(),error=connectionError();
 function addKey(key='',existing=false){
  const row=node('div','connection-value-row'),keyField=field('Key','',key,'text',{required:'',maxlength:'128',pattern:'[A-Za-z0-9_.-]+'}),value=field(existing?'Replacement value':'Value','', '', 'password',{autocomplete:'new-password',placeholder:existing?'Leave blank to keep saved value':'Enter a value'}),remove=node('button','','Remove');
  keyField.querySelector('input').readOnly=existing;row.dataset.existing=String(existing);remove.type='button';
  remove.addEventListener('click',()=>{if(existing)removed.add(key);row.remove();form.dataset.dirty='true';});
  row.append(keyField,value,remove);rows.append(row);
 }
 for(const key of connection?.keys||['host','port','database','username','password'])addKey(key,Boolean(connection));
 const add=node('button','','＋ Add key');add.type='button';add.addEventListener('click',()=>addKey());
 const save=node('button','primary',connection?'Save connection':'Create connection');save.type='submit';
 if(connection)form.append(connectionUsage(connection));
 form.append(grid(name,typeLabel),node('p','settings-note','Values are stored in a Kubernetes Secret and are never returned to this screen. Add any keys your app needs, including URLs, certificates or tokens.'),rows,add,node('p','settings-note','This saves connection configuration; it does not deploy a database or change credentials inside an existing database. Kubernetes encryption at rest is configured separately on your cluster.'),error,save);
 form.addEventListener('input',()=>form.dataset.dirty='true');
 form.addEventListener('submit',async event=>{
  event.preventDefault();save.disabled=true;error.textContent='';
  try{
   const values={},seen=new Set();
   for(const row of rows.children){const [keyInput,valueInput]=row.querySelectorAll('input'),key=keyInput.value.trim();if(seen.has(key))throw new Error('Duplicate key: '+key);seen.add(key);if(row.dataset.existing!=='true'||valueInput.value!=='')values[key]=valueInput.value;}
   await apiOnCluster(cluster,`/api/projects/${encodeURIComponent(project)}/connections${connection?'/'+encodeURIComponent(connection.id):''}`,{name:name.querySelector('input').value,kind:type.value,project_id:project,values,remove_keys:[...removed],version:connection?.version||null});
   for(const input of rows.querySelectorAll('input[type=password]'))input.value='';delete form.dataset.dirty;dialog.close();notify('Connection saved. Review application status for any required restarts.');await onSaved();
  }catch(e){error.textContent=e.message;}finally{save.disabled=false;}
 });body.append(form);
}
function connectionUsage(connection,app) {
 const root=node('div','connection-usage');
 if(connection.usage_error){root.append(node('p','form-error',connection.usage_error));return root;}
 const consumers=connection.consumers||[],visible=app?consumers.filter(c=>c.name===app):consumers;
 root.append(node('p','settings-note',consumers.length?`Used by ${consumers.length} application${consumers.length===1?'':'s'}`:'Not attached to any project applications.'));
 for(const consumer of visible){const row=node('div','connection-consumer');row.append(node('strong','',consumer.name),node('span',consumer.restart_required?'badge connection-stale':'badge',consumer.status));root.append(row);}
 if(visible.some(c=>c.restart_required))root.append(node('p','connection-warning','Saved connection values have changed. Restart the affected apps to load them; paused apps load them on their next start.'));
 return root;
}
function projectConnections(project) {
 const cluster=activeClusterId,{dialog,body}=connectionDialog('Connections & secrets'),error=connectionError(),list=node('div'),actions=node('div','cluster-actions');let generation=0;
 const add=node('button','primary','＋ Add connection');add.type='button';add.addEventListener('click',()=>connectionEditor(project,cluster,null,load));
 const refresh=node('button','','Refresh');refresh.addEventListener('click',load);actions.append(add,refresh);
 body.append(node('p','settings-note','Create named connections, then attach them from an application’s Environment tab. Several apps can share a connection. Projects organize access in KubeWeasel; they are not Kubernetes isolation boundaries.'),actions,error,list);
 async function load(){
  const request=++generation;error.textContent='';try{
   const data=await apiOnCluster(cluster,`/api/projects/${encodeURIComponent(project)}/connections`);if(!dialog.isConnected||cluster!==activeClusterId||request!==generation)return;
   list.replaceChildren();
   for(const connection of data.connections){
    const card=node('div','storage-mount');card.append(node('strong','',connection.name),node('span','badge',connectionTypeLabel(connection.kind)),node('p','',`${connection.keys.length} saved keys · values hidden`));
    card.append(connectionUsage(connection));
    const buttons=node('div','cluster-actions'),edit=node('button','','Edit'),restart=node('button','','Restart connected apps'),remove=node('button','','Delete');
    edit.addEventListener('click',()=>connectionEditor(project,cluster,connection,load));
    restart.addEventListener('click',async()=>{if(!confirm(`Restart apps using “${connection.name}”: ${(connection.consumers||[]).map(c=>c.name).join(', ')||'none'}?`))return;restart.disabled=true;try{const result=await apiOnCluster(cluster,`/api/projects/${project}/connections/${connection.id}/restart`,{version:connection.version});await load();error.textContent=result.failed.length?`Restart failed for: ${result.failed.join(', ')}. Other restart requests were retained.`:'';notify(`Restart requested for ${result.restarted.length} application(s).`);}catch(e){error.textContent=e.message;}finally{restart.disabled=false;}});
    remove.addEventListener('click',async()=>{if(!confirm(`Delete connection “${connection.name}”? This keeps database services and data.`))return;remove.disabled=true;try{await apiOnCluster(cluster,`/api/projects/${project}/connections/${connection.id}`,{version:connection.version},'DELETE');await load();}catch(e){error.textContent=e.message;}finally{remove.disabled=false;}});
    restart.disabled=Boolean(connection.usage_error)||!(connection.consumers||[]).length;buttons.append(edit,restart,remove);card.append(buttons);list.append(card);
   }
   if(!data.connections.length)list.append(node('p','empty','No connections yet. Add Postgres, MySQL, Kafka or a custom set of secrets.'));
  }catch(e){error.textContent=e.message;}
 }load();
 const timer=setInterval(()=>{if(!dialog.isConnected){clearInterval(timer);return;}if(!document.hidden&&cluster===activeClusterId)load();},10000);
 dialog.addEventListener('close',()=>clearInterval(timer),{once:true});
}
function appConnectionsPanel(){
 let generation=0;
 const root=panel('Connections & secrets','Attach multiple named connections. Each value is injected through a Kubernetes Secret reference; attaching or detaching restarts the application.'),app=detailName,cluster=activeClusterId,project=detailData.config.project_id,error=connectionError(),list=node('div');
 if(!project){root.append(node('p','settings-note','Move this application into a project to attach connections.'));return root;}
 const manage=node('button','','Manage project connections');manage.addEventListener('click',()=>projectConnections(project));const refresh=node('button','','Refresh connections');refresh.addEventListener('click',()=>{if(root.querySelector('[data-dirty=true]')&&!confirm('Discard unsaved connection mappings and refresh?'))return;load();});const actions=node('div','cluster-actions');actions.append(manage,refresh);root.append(actions,error,list);
 async function load(){
  const request=++generation;try{const data=await apiOnCluster(cluster,`/api/apps/${encodeURIComponent(app)}/connections`);if(!root.isConnected||cluster!==activeClusterId||request!==generation)return;list.replaceChildren();error.textContent='';
   for(const connection of data.connections){
    const card=node('div','storage-mount'),attached=Object.keys(connection.mappings).length>0;card.append(node('strong','',connection.name),node('span','badge',attached?'Attached':connectionTypeLabel(connection.kind)));
    card.append(connectionUsage(connection,app));
    const form=node('form','settings-form'),rows=node('div');
    function mapping(variable='',key=connection.keys[0]){const row=node('div','connection-mapping-row'),name=field('Environment variable','',variable,'text',{required:'',pattern:'[A-Za-z_][A-Za-z0-9_]*',maxlength:'128',placeholder:'POSTGRES_URL'}),label=node('label','setting-field','Connection key'),select=node('select','project-select');for(const k of connection.keys){const option=node('option','',k);option.value=k;select.append(option);}select.value=key;label.append(select);const remove=node('button','','Remove');remove.type='button';remove.addEventListener('click',()=>{row.remove();form.dataset.dirty='true';});row.append(name,label,remove);rows.append(row);}
    if(attached)for(const [env,key]of Object.entries(connection.mappings))mapping(env,key);
    else {const preferred=connection.keys.includes('url')?'url':connection.keys[0];mapping((connection.name+'_'+preferred).replace(/[^A-Za-z0-9_]/g,'_').toUpperCase().replace(/^(?=\d)/,'_'),preferred);}
    const add=node('button','','＋ Map another variable');add.type='button';add.addEventListener('click',()=>{mapping();form.dataset.dirty='true';});
    const save=node('button','primary',attached?'Save mappings':'Attach connection');save.type='submit';const mappingActions=node('div','cluster-actions');mappingActions.append(save);form.append(rows,add,mappingActions);form.addEventListener('input',()=>form.dataset.dirty='true');form.addEventListener('change',()=>form.dataset.dirty='true');
    form.addEventListener('submit',async event=>{event.preventDefault();save.disabled=true;error.textContent='';try{const mappings={};for(const row of rows.children){const name=row.querySelector('input').value.trim();if(Object.hasOwn(mappings,name))throw new Error('Duplicate variable: '+name);mappings[name]=row.querySelector('select').value;}if(!Object.keys(mappings).length)throw new Error('Add a mapping or use Detach.');await apiOnCluster(cluster,`/api/apps/${encodeURIComponent(app)}/connections/${connection.id}`,{mappings,version:connection.version,workload_version:connection.workload_version});delete form.dataset.dirty;notify('Connection attached; application rollout requested.');await load();}catch(e){error.textContent=e.message;}finally{save.disabled=false;}});
    if(attached){const detach=node('button','','Detach connection');detach.type='button';detach.addEventListener('click',async()=>{if(!confirm('Detach these environment variables and restart this application? The connection and database data are retained.'))return;detach.disabled=true;try{await apiOnCluster(cluster,`/api/apps/${encodeURIComponent(app)}/connections/${connection.id}`,{mappings:{},version:connection.version,workload_version:connection.workload_version},'DELETE');await load();notify('Connection detached; stored credentials retained.');}catch(e){error.textContent=e.message;}finally{detach.disabled=false;}});mappingActions.append(detach);}
    card.append(form);list.append(card);
   }if(!data.connections.length)list.append(node('p','settings-note','No connections in this project. Use Manage project connections to add one.'));
  }catch(e){if(root.isConnected)error.textContent=e.message;}
 }setTimeout(load,0);const timer=setInterval(()=>{if(!root.isConnected){clearInterval(timer);return;}if(!document.hidden&&!root.querySelector('[data-dirty=true]')&&cluster===activeClusterId)load();},10000);return root;
}
