/* Headphone DSP — dependency-free, offline studio UI.
 * Native responses are authoritative. Browser mode is a visual simulation only;
 * its analytical crossfeed approximation is not a replacement for KU100 data.
 */
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const clone = value => JSON.parse(JSON.stringify(value));
  const automaticPreamp = value => ({...clone(value), auto_preamp:true, preamp_db:0, eq_bands:normalizeGeq(value.eq_bands,value.sample_rate)});
  const clamp = (v, min, max) => Math.min(max, Math.max(min, v));
  const signed = (n, digits = 1) => `${n < 0 ? '−' : '+'}${Math.abs(n).toFixed(digits)}`;
  const dspSampleRate = rate => (rate > 0 && rate % 44100 === 0) ? 44100 : (rate > 0 && rate % 48000 === 0) ? 48000 : (rate < 46050 ? 44100 : 48000);
  const formatRate = rate => {
    const dsp = dspSampleRate(rate);
    return rate !== dsp ? `${rate/1000} → ${dsp/1000} kHz` : `${dsp/1000} kHz`;
  };
  const colors = ['#7dd3c8', '#e8b34b', '#c59af2', '#e68b9b', '#85a9ee', '#bed583', '#e6a878', '#86c5e5', '#d995d1', '#8fd6ae', '#c6b88e', '#a8a0e8'];
  const geqFreqs = [31,62,125,250,500,1000,2000,4000,8000,16000];
  const geqLabels = ['31','62','125','250','500','1k','2k','4k','8k','16k'];
  const geqQ = 1.41;
  const geqMaxGain = 6;
  const makeGeqBands = (gains=[]) => geqFreqs.map((freq_hz,i)=>({enabled:true,band_type:'bell',freq_hz,gain_db:clamp(Number.isFinite(gains[i])?gains[i]:0,-geqMaxGain,geqMaxGain),q:geqQ}));
  const gainTypes = new Set(['bell', 'low_shelf', 'high_shelf']);
  const previewMode = !window.__TAURI__?.core?.invoke;
  const zoomStorageKey = 'headphone-dsp.ui-zoom.v1';
  let uiZoom = 1, zoomQueue = Promise.resolve(), zoomRevision = 0;

  function setUIZoom(value, persist = true) {
    if (!Number.isFinite(value)) value = 1;
    uiZoom = Math.round(clamp(value, .7, 2) * 100) / 100;
    if (persist) {
      try { localStorage.setItem(zoomStorageKey, String(uiZoom)); } catch { /* Zoom still works without storage. */ }
    }
    if (previewMode) {
      document.documentElement.style.zoom = String(uiZoom);
    } else {
      const zoom = uiZoom, expected = ++zoomRevision;
      // Serialize native calls and coalesce waiting wheel events so older zoom
      // requests cannot complete after, and replace, the latest choice.
      zoomQueue = zoomQueue.then(async () => {
        if (expected !== zoomRevision) return;
        await window.__TAURI__.core.invoke('set_webview_zoom', { zoom });
      }).catch(error => toast(`UI zoom could not be applied: ${error?.message || String(error)}`, 'error'));
    }
  }

  window.addEventListener('keydown', event => {
    if (!(event.ctrlKey || event.metaKey) || event.altKey) return;
    let next;
    if (event.key === '+' || event.key === '=' || event.code === 'NumpadAdd') next = uiZoom + .1;
    else if (event.key === '-' || event.key === '_' || event.code === 'NumpadSubtract') next = uiZoom - .1;
    else if (event.key === '0' || event.code === 'Numpad0') next = 1;
    else return;
    event.preventDefault();
    setUIZoom(next);
  }, { passive: false });
  window.addEventListener('wheel', event => {
    if (!(event.ctrlKey || event.metaKey)) return;
    event.preventDefault();
    if (!event.deltaY) return;
    const pixels = event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? window.innerHeight : 1);
    setUIZoom(uiZoom + clamp(-pixels * .001, -.1, .1));
  }, { passive: false });
  const defaults = {
    capture_backend:'Pulse', capture_device:'headphone_dsp_sink.monitor', capture_loopback:true,
    playback_backend:'Alsa', playback_device:'plughw:CARD=Generic_1,DEV=0', exclusive_mode:false, hw_volume_bypass:false,
    sample_rate:48000, chunksize:512, master_bypass:false, preamp_db:0, auto_preamp:true,
    crossfeed_enabled:true, crossfeed_angle_deg:35, crossfeed_mid_boost_db:3, eq_enabled:true,
    eq_bands:makeGeqBands([3.5,3,1.5,0,0,0,0,-.5,-1.5,-1.5]), active_profile:'Default'
  };
  let state = null, settings = clone(defaults), revision = 0, selectedBand = -1;
  let updateTimer, queue = Promise.resolve(), pollTimer, pollBusy = false, pollGeneration = 0;
  let ready = false, drag = null, frame = null, toastError = '', clipAcknowledged = 0;
  let mockSettings = clone(defaults), mockRunning = false, mockClips = 0;
  let profileAction = 'save', engineBusy = false, actionDepth = 0;
  const storageKey = 'headphone-dsp.preview.profiles.v1';
  const builtins = {
    'Flat / Neutral': {...clone(defaults), eq_bands:makeGeqBands(), crossfeed_enabled:false},
    'Warm Vocal & Sub-Bass': {...clone(defaults), eq_bands:makeGeqBands([4.5,4,2,0,0,.5,1.5,0,-2,-2])},
    'HD 650 / 6XX Harman Target': {...clone(defaults), eq_bands:makeGeqBands([5.5,5,2.5,0,0,0,-1,-2,2,2])}
  };
  let mockProfiles = clone(builtins);
  try { const stored = JSON.parse(localStorage.getItem(storageKey) || 'null'); if (stored && typeof stored === 'object' && !Array.isArray(stored)) mockProfiles = {...stored}; } catch { /* Private browser / invalid storage: use defaults. */ }

  // Robert Bristow-Johnson audio EQ cookbook biquads, normalized by a0.
  function coefficients(band, rate) {
    const w = 2 * Math.PI * clamp(band.freq_hz, 1, rate * .499) / rate;
    const c = Math.cos(w), s = Math.sin(w), q = Math.max(.05, band.q);
    const a = 10 ** (band.gain_db / 40), alpha = s / (2 * q), beta = 2 * Math.sqrt(a) * alpha;
    let b0, b1, b2, a0, a1, a2;
    switch (band.band_type) {
      case 'low_shelf':
        b0=a*((a+1)-(a-1)*c+beta); b1=2*a*((a-1)-(a+1)*c); b2=a*((a+1)-(a-1)*c-beta);
        a0=(a+1)+(a-1)*c+beta; a1=-2*((a-1)+(a+1)*c); a2=(a+1)+(a-1)*c-beta; break;
      case 'high_shelf':
        b0=a*((a+1)+(a-1)*c+beta); b1=-2*a*((a-1)+(a+1)*c); b2=a*((a+1)+(a-1)*c-beta);
        a0=(a+1)-(a-1)*c+beta; a1=2*((a-1)-(a+1)*c); a2=(a+1)-(a-1)*c-beta; break;
      case 'high_pass': b0=(1+c)/2; b1=-(1+c); b2=b0; a0=1+alpha; a1=-2*c; a2=1-alpha; break;
      case 'low_pass': b0=(1-c)/2; b1=1-c; b2=b0; a0=1+alpha; a1=-2*c; a2=1-alpha; break;
      case 'notch': b0=1; b1=-2*c; b2=1; a0=1+alpha; a1=-2*c; a2=1-alpha; break;
      default: b0=1+alpha*a; b1=-2*c; b2=1-alpha*a; a0=1+alpha/a; a1=-2*c; a2=1-alpha/a;
    }
    return [b0/a0,b1/a0,b2/a0,a1/a0,a2/a0];
  }
  function responseAt(coeff, frequency, rate) {
    const w = 2*Math.PI*frequency/rate, c = Math.cos(w), s = Math.sin(w), c2 = Math.cos(2*w), s2 = Math.sin(2*w);
    const [b0,b1,b2,a1,a2] = coeff;
    const numerator = (b0+b1*c+b2*c2)**2 + (b1*s+b2*s2)**2;
    const denominator = (1+a1*c+a2*c2)**2 + (a1*s+a2*s2)**2;
    return 10*Math.log10(Math.max(1e-16,numerator)/Math.max(1e-16,denominator));
  }
  function computePEQ(s) {
    const rate = dspSampleRate(s.sample_rate);
    const freq_hz = Array.from({length:280}, (_,i) => 20 * 1000**(i/279));
    const per_band_db = s.eq_bands.map(band => {
      const coeff = coefficients(band,rate);
      return freq_hz.map(f => band.enabled ? responseAt(coeff,f,rate) : 0);
    });
    const composite_db = freq_hz.map((_,i) => s.eq_enabled ? per_band_db.reduce((sum,values) => sum+values[i],0) : 0);
    const raw_peak_db = Math.max(0,...composite_db);
    const effective_preamp_db = s.master_bypass ? 0 : s.preamp_db - (s.auto_preamp ? Math.max(0,raw_peak_db) : 0);
    return {freq_hz,per_band_db,composite_db,raw_peak_db,effective_preamp_db};
  }
  function fitGeq(target,rate) {
    const bands=makeGeqBands();
    // Fit the sampled target, including overlap between neighboring bells.
    for(let pass=0;pass<16;pass++) geqFreqs.forEach((f,i)=>{
      const current=bands.reduce((sum,b)=>sum+responseAt(coefficients(b,rate),f,rate),0);
      bands[i].gain_db=clamp(bands[i].gain_db+.7*(target[i]-current),-geqMaxGain,geqMaxGain);
    });
    return makeGeqBands(bands.map(b=>Math.round(b.gain_db*10)/10));
  }
  function normalizeGeq(bands=[],sourceRate=48000) {
    if(bands.length===10 && bands.every((b,i)=>b.band_type==='bell' && b.freq_hz===geqFreqs[i] && b.q===geqQ))
      return makeGeqBands(bands.map(b=>b.enabled?b.gain_db:0));
    const rate=dspSampleRate(sourceRate);
    return fitGeq(geqFreqs.map(f=>bands.reduce((sum,b)=>sum+(b.enabled?responseAt(coefficients(b,rate),f,rate):0),0)),rate);
  }
  function computeCrossfeed(s) {
    const freq_hz = Array.from({length:110},(_,i)=>20*1000**(i/109));
    const angle = s.crossfeed_angle_deg * Math.PI/180, delay = .00064*Math.sin(angle);
    const mid_db=[], side_db=[], ild_db=[];
    freq_hz.forEach(f => {
      // A simple frequency-dependent opposite-ear transfer with interaural delay.
      // This deliberately approximates, rather than claims to reproduce, KU100 HRTFs.
      const cross = .48 / Math.sqrt(1+(f/(900+900*Math.cos(angle)))**2);
      const phase = 2*Math.PI*f*delay;
      const normalization = 1.48;
      const mid = Math.sqrt(Math.max(1e-12,1+cross*cross+2*cross*Math.cos(phase)))/normalization;
      const side = Math.sqrt(Math.max(1e-12,1+cross*cross-2*cross*Math.cos(phase)))/normalization;
      const boost = s.crossfeed_mid_boost_db*(1-Math.exp(-f/600))*Math.exp(-f/14000);
      mid_db.push(s.crossfeed_enabled ? 20*Math.log10(mid)+boost : 0);
      side_db.push(s.crossfeed_enabled ? 20*Math.log10(side) : 0);
      ild_db.push(s.crossfeed_enabled ? Math.min(24,-20*Math.log10(Math.max(.001,cross))) : 0);
    });
    return {freq_hz,mid_db,side_db,ild_db};
  }
  function mockMeters() {
    const base = mockRunning ? -17+Math.sin(Date.now()/900)*4 : -120;
    const input = [base,base-1.8+Math.sin(Date.now()/740)*1.5];
    const preamp = computePEQ(mockSettings).effective_preamp_db;
    const output = input.map(v=>mockRunning?v+preamp:-120);
    return {running:mockRunning,state:mockRunning?'Running':'Stopped',sample_rate:mockSettings.sample_rate,chunksize:mockSettings.chunksize,capture_peak:input,playback_peak:output,capture_rms:input.map(v=>v-8),playback_rms:output.map(v=>v-8),load_percent:mockRunning?2.4+Math.sin(Date.now()/1500)*.5:0,clipped_samples:mockClips,error:null};
  }
  function mockResponse() {
    return {settings:clone(mockSettings),capture_devices:[{id:'headphone_dsp_sink.monitor',label:'[Virtual Sink Loopback] Headphone DSP Virtual Input',backend:'Pulse',is_loopback:true,channels:2}],playback_devices:[{id:'plughw:CARD=Generic_1,DEV=0',label:'ALC257 Analog (ALSA Direct)',backend:'Alsa',is_loopback:false,channels:2},{id:'default',label:'System default output',backend:'Pulse',is_loopback:false,channels:2}],profiles:Object.keys(mockProfiles),peq_preview:computePEQ(mockSettings),crossfeed_preview:computeCrossfeed(mockSettings),meters:mockMeters()};
  }
  function persistProfiles() {
    try { localStorage.setItem(storageKey,JSON.stringify(mockProfiles)); }
    catch { toast('Profile is available for this session only; browser storage is unavailable.'); }
  }
  function parseAutoEQ(text) {
    const bands = []; let preamp = null;
    const graphic=text.match(/^\s*GraphicEQ:\s*(.+)$/im);
    if(graphic) {
      const points=graphic[1].split(';').map(pair=>pair.trim().split(/\s+/).map(Number));
      if(points.some(p=>p.length!==2 || !p.every(Number.isFinite) || p[0]<=0)) throw Error('Invalid GraphicEQ frequency / gain pair.');
      points.sort((a,b)=>a[0]-b[0]);
      if(points.some((p,i)=>i && p[0]===points[i-1][0])) throw Error('GraphicEQ frequencies must be unique.');
      const target=geqFreqs.map(f=>{
        if(f<=points[0][0]) return points[0][1];
        const hi=points.findIndex(p=>p[0]>=f);
        if(hi<0) return points.at(-1)[1];
        const a=points[hi-1],b=points[hi];
        return a[1]+(b[1]-a[1])*Math.log(f/a[0])/Math.log(b[0]/a[0]);
      });
      return {bands:fitGeq(target,dspSampleRate(settings.sample_rate)),preamp:null};
    }
    const codes = {PK:'bell',PEQ:'bell',LS:'low_shelf',LSC:'low_shelf',HS:'high_shelf',HSC:'high_shelf',HP:'high_pass',HPQ:'high_pass',LP:'low_pass',LPQ:'low_pass',NO:'notch',NOTCH:'notch'};
    const number = '([+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+))';
    for (const raw of text.split(/\r?\n/)) {
      const line = raw.replace(/#.*/, '').trim(); if (!line) continue;
      const p = line.match(new RegExp('^Preamp:\\s*'+number+'\\s*dB\\s*$','i'));
      if (p) { preamp=Number(p[1]); if (preamp < -18 || preamp > 12) throw Error('Preamp must be between −18 and +12 dB.'); continue; }
      if (!/^Filter\s/i.test(line)) continue;
      const head = line.match(/^Filter\s+\d+:\s*(ON|OFF)\s+([A-Z]+)\b/i);
      if (!head || !codes[head[2].toUpperCase()]) throw Error(`Unsupported filter: ${line}`);
      const type = codes[head[2].toUpperCase()];
      const f = line.match(new RegExp('\\bFc\\s+'+number+'\\s*Hz','i'));
      const g = line.match(new RegExp('\\bGain\\s+'+number+'\\s*dB','i'));
      const q = line.match(new RegExp('\\bQ\\s+'+number+'(?:\\s|$)','i'));
      if (!f || (gainTypes.has(type) && !g)) throw Error(`Missing frequency or gain: ${line}`);
      const band = {enabled:head[1].toUpperCase()==='ON',band_type:type,freq_hz:Number(f[1]),gain_db:g?Number(g[1]):0,q:q?Number(q[1]):.7071};
      if (band.freq_hz<20 || band.freq_hz>20000 || Math.abs(band.gain_db)>18 || band.q<.1 || band.q>20) throw Error('Filters must use 20–20,000 Hz, ±18 dB, and Q 0.1–20.');
      bands.push(band);
    }
    if (!bands.length && preamp === null) throw Error('No supported filters or preamp found. Paste AutoEQ filter text.');
    if (bands.length>128) throw Error('A preset can contain at most 128 filters.');
    return {bands:normalizeGeq(bands,settings.sample_rate),preamp};
  }
  async function mockInvoke(command,args={}) {
    switch(command) {
      case 'get_live_meters': return mockMeters();
      case 'update_dsp_settings': mockSettings=automaticPreamp(args.settings); break;
      case 'start_engine': mockSettings=automaticPreamp(args.settings); mockRunning=true; break;
      case 'stop_engine': mockRunning=false; break;
      case 'save_profile': Object.defineProperty(mockProfiles,args.name,{value:clone(mockSettings),enumerable:true,writable:true,configurable:true}); mockSettings.active_profile=args.name; persistProfiles(); break;
      case 'load_profile': if (!Object.hasOwn(mockProfiles,args.name)) throw Error('Profile not found.'); mockSettings=automaticPreamp({...mockSettings,...mockProfiles[args.name],active_profile:args.name}); break;
      case 'delete_profile': delete mockProfiles[args.name]; if (mockSettings.active_profile===args.name) mockSettings.active_profile='Custom'; persistProfiles(); break;
      case 'import_autoeq': {
        const parsed=parseAutoEQ(args.text); mockSettings.eq_bands=parsed.bands; mockSettings.eq_enabled=true;
        mockSettings.preamp_db=0; mockSettings.auto_preamp=true;
        mockSettings.active_profile='Imported AutoEQ'; break;
      }
      case 'export_autoeq': {
        const code={bell:'PK',low_shelf:'LS',high_shelf:'HS',high_pass:'HPQ',low_pass:'LPQ',notch:'NO'};
        return {text:`Preamp: ${computePEQ(mockSettings).effective_preamp_db.toFixed(2)} dB\n`+mockSettings.eq_bands.map((b,i)=>`Filter ${i+1}: ${b.enabled?'ON':'OFF'} ${code[b.band_type]} Fc ${b.freq_hz.toFixed(1)} Hz${gainTypes.has(b.band_type)?` Gain ${b.gain_db.toFixed(2)} dB`:''} Q ${b.q.toFixed(3)}`).join('\n')};
      }
    }
    return mockResponse();
  }
  const invoke = (command,args) => previewMode ? mockInvoke(command,args) : window.__TAURI__.core.invoke(command,args);
  function enqueue(task) { const next=queue.then(task); queue=next.catch(()=>{}); return next; }
  function toast(message,kind='success') {
    const el=document.createElement('div'); el.className=`toast ${kind}`; el.textContent=message; $('toasts').append(el);
    setTimeout(()=>el.remove(),kind==='error'?9000:4500);
  }
  function report(error) {
    const message=typeof error==='string'?error:error?.message||String(error);
    $('sync-status').textContent='Not synced · edit a setting to retry';
    if(message!==toastError) { toast(message,'error'); toastError=message; setTimeout(()=>{toastError='';},5000); }
  }
  function fillRange(el) { el.style.setProperty('--fill',`${100*(Number(el.value)-Number(el.min))/(Number(el.max)-Number(el.min))}%`); }
  function setRange(id,value) { $(id).value=value; fillRange($(id)); }
  function selectDevices(id,devices,value) {
    const select=$(id); select.replaceChildren();
    devices.forEach(device=>select.add(new Option(device.label,device.id)));
    if(!devices.some(d=>d.id===value)) select.add(new Option(value?`Unavailable · ${value}`:'No device selected',value||''));
    select.value=value;
  }
  function renderDevices() {
    const caps=state.capture_devices||[];
    const singleCapture=caps.length===1;
    if(singleCapture && settings.capture_device!==caps[0].id) {
      settings.capture_device=caps[0].id;
      settings.capture_backend=caps[0].backend;
      settings.capture_loopback=caps[0].is_loopback;
    }
    $('capture-wrap').hidden=singleCapture;
    $('capture-arrow').hidden=singleCapture;
    selectDevices('capture',caps,settings.capture_device);
    selectDevices('playback',state.playback_devices||[],settings.playback_device);
  }
  function renderProfiles() {
    $('profiles').replaceChildren();
    const profiles=state.profiles||[];
    if(!profiles.includes(settings.active_profile)) $('profiles').add(new Option(settings.active_profile||'Custom',settings.active_profile||''));
    profiles.forEach(name=>$('profiles').add(new Option(name,name)));
    $('profiles').value=settings.active_profile||'';
    $('delete-profile').disabled=!profiles.includes(settings.active_profile);
  }
  function renderSettings() {
    $('crossfeed-enabled').checked=settings.crossfeed_enabled;
    $('eq-enabled').checked=settings.eq_enabled;
    setRange('angle',settings.crossfeed_angle_deg); setRange('mid',settings.crossfeed_mid_boost_db);
    $('exclusive').checked=settings.exclusive_mode;
    updateLabels(); renderDevices(); renderProfiles(); renderBands();
  }
  function updateLabels() {
    $('angle-value').innerHTML=`${settings.crossfeed_angle_deg.toFixed(0)}<span>°</span>`;
    $('mid-value').innerHTML=`${settings.crossfeed_mid_boost_db.toFixed(1)} <span>dB</span>`;
    $('bypass').setAttribute('aria-pressed',String(settings.master_bypass)); $('bypass-label').textContent=settings.master_bypass?'Original signal':'DSP active';
    $('hw-vol').setAttribute('aria-pressed',String(!!settings.hw_volume_bypass)); $('hw-vol-label').textContent=settings.hw_volume_bypass?'Fixed 0 dB':'OS Vol';
    $('bypass-overlay').hidden=!settings.master_bypass;
    $('crossfeed-card').classList.toggle('processing-off',!settings.crossfeed_enabled);
    $('exclusive-wrap').hidden=!(settings.capture_backend.toLowerCase()==='wasapi'||settings.playback_backend.toLowerCase()==='wasapi');
    const dspRate = dspSampleRate(settings.sample_rate);
    $('latency').textContent=`${formatRate(settings.sample_rate)} · ${settings.chunksize} samples · ${(settings.chunksize/dspRate*1000).toFixed(1)} ms buffer`;
    document.querySelectorAll('[data-angle]').forEach(el=>el.classList.toggle('active',Number(el.dataset.angle)===settings.crossfeed_angle_deg));
  }
  function renderBands() {
    $('band-rows').innerHTML=settings.eq_bands.map((b,i)=>`<div class="geq-column" data-row="${i}" style="--band-color:${colors[i]}"><input type="number" class="geq-num" data-band="${i}" value="${b.gain_db.toFixed(1)}" min="-6" max="6" step="0.1" aria-label="${geqFreqs[i]} Hz gain in dB"><div class="geq-fader" data-fader="${i}" tabindex="0" role="slider" aria-label="${geqFreqs[i]} Hz fader" aria-orientation="vertical" aria-valuemin="-6" aria-valuemax="6" aria-valuenow="${b.gain_db.toFixed(1)}">${[6,3,0,-3,-6].map(v=>`<span class="geq-tick ${v===0?'zero':''}" style="top:${(6-v)/12*100}%" aria-hidden="true"><small>${v>0?'+':''}${v}</small></span>`).join('')}<i class="geq-fill" aria-hidden="true"></i><b class="geq-cap" aria-hidden="true"></b></div><span class="geq-freq">${geqLabels[i]}<small>Hz</small></span></div>`).join('');
    settings.eq_bands.forEach((_,i)=>syncBandRow(i));
    $('band-count').textContent='10-Band ISO · Q 1.41';
  }
  function syncBandRow(index) {
    const b=settings.eq_bands[index]; if(!b) return;
    const row=$('band-rows').querySelector(`[data-row="${index}"]`); if(!row) return;
    const input=row.querySelector('input'),fader=row.querySelector('.geq-fader'),position=(6-clamp(b.gain_db,-geqMaxGain,geqMaxGain))/12*100;
    if(document.activeElement!==input) input.value=b.gain_db.toFixed(1);
    fader.setAttribute('aria-valuenow',b.gain_db.toFixed(1));
    fader.setAttribute('aria-valuetext',`${signed(b.gain_db)} dB`);
    fader.querySelector('.geq-cap').style.top=`${position}%`;
    const fill=fader.querySelector('.geq-fill');fill.style.top=`${Math.min(50,position)}%`;fill.style.height=`${Math.abs(position-50)}%`;
    row.classList.toggle('selected',selectedBand===index);
  }
  const graph={width:800,height:136,left:52,right:778,top:14,bottom:110};
  const gx=f=>graph.left+Math.log10(clamp(f,20,20000)/20)/3*(graph.right-graph.left);
  const gy=db=>graph.top+(12-clamp(db,-14,14))/24*(graph.bottom-graph.top);
  function pathFor(freq,values,x=gx,y=gy) { return freq.map((f,i)=>`${i?'L':'M'}${x(f).toFixed(2)},${y(Number.isFinite(values[i])?values[i]:0).toFixed(2)}`).join(' '); }
  function renderEQ(preview) {
    if(!preview?.freq_hz?.length) return;
    const ticks=[20,50,100,200,500,1000,2000,5000,10000,20000], levels=[-12,-6,0,6,12];
    let html='<defs><linearGradient id="eq-fill" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#b98af0" stop-opacity=".2"/><stop offset="100%" stop-color="#b98af0" stop-opacity=".015"/></linearGradient><clipPath id="plot-clip"><rect x="52" y="14" width="726" height="96"/></clipPath></defs>';
    levels.forEach(db=>{const y=gy(db);html+=`<line x1="52" x2="778" y1="${y}" y2="${y}" stroke="${db===0?'#83899775':'#ffffff09'}" ${db===0?'stroke-dasharray="3 5"':''}/><text x="42" y="${y+3.5}" text-anchor="end" fill="#838997" font-size="11">${db>0?'+':''}${db}</text>`;});
    ticks.forEach(f=>{const x=gx(f);html+=`<line x1="${x}" x2="${x}" y1="14" y2="110" stroke="#ffffff08"/><text x="${x}" y="128" text-anchor="middle" fill="#838997" font-size="11">${f>=1000?`${f/1000}k`:f}</text>`;});
    html+='<text x="6" y="65.5" fill="#838997" font-size="10">dB</text><g clip-path="url(#plot-clip)">';
    const compositePath=pathFor(preview.freq_hz,preview.composite_db);
    html+=`<path d="${compositePath} L${gx(20000)},${gy(0)} L${gx(20)},${gy(0)} Z" fill="url(#eq-fill)"/>`;
    (preview.per_band_db||[]).forEach((values,i)=>{if(settings.eq_bands[i]?.enabled && settings.eq_enabled) html+=`<path d="${pathFor(preview.freq_hz,values)}" fill="none" stroke="${colors[i]}" stroke-width="1.15" opacity="${selectedBand===i?.7:.3}"/>`;});
    html+=`<path d="${compositePath}" fill="none" stroke="#b98af0" stroke-width="2.5" stroke-linejoin="round" opacity="${settings.eq_enabled?1:.3}"/></g>`;
    settings.eq_bands.forEach((b,i)=>{
      if(!b.enabled) return;
      const y=gy(gainTypes.has(b.band_type)?b.gain_db:0),x=gx(b.freq_hz);
      html+=`<g class="handle" data-handle="${i}" opacity="${settings.eq_enabled?1:.4}"><circle cx="${x}" cy="${y}" r="19" fill="transparent"/>${selectedBand===i?`<circle cx="${x}" cy="${y}" r="14" fill="${colors[i]}" opacity=".12"/>`:''}<circle cx="${x}" cy="${y}" r="9" fill="#171b22" stroke="${colors[i]}" stroke-width="1.5"/><text x="${x}" y="${y+3}" text-anchor="middle" fill="${colors[i]}" font-size="11" pointer-events="none">${i+1}</text></g>`;
    });
    $('eq-graph').innerHTML=html;
    $('peak').textContent=`${signed(preview.raw_peak_db||0)} dB`;
    $('effective').textContent=`${signed(preview.effective_preamp_db||0)} dB`;
    $('effective').style.color=preview.effective_preamp_db+(preview.raw_peak_db||0)>0?'var(--warn)':'var(--ok)';
  }
  function renderStage() {
    const a=settings.crossfeed_angle_deg*Math.PI/180,cx=150,cy=111,r=91;
    const lx=cx-Math.sin(a)*r,rx=cx+Math.sin(a)*r,y=cy-Math.cos(a)*r;
    $('stage').innerHTML=`<defs><radialGradient id="stage-glow"><stop stop-color="#b98af0" stop-opacity=".09"/><stop offset="1" stop-color="#b98af0" stop-opacity="0"/></radialGradient></defs><ellipse cx="150" cy="92" rx="100" ry="66" fill="url(#stage-glow)"/><path d="M59 111 A91 91 0 0 1 241 111" fill="none" stroke="#ffffff12" stroke-dasharray="3 5"/><path d="M${lx} ${y} A91 91 0 ${settings.crossfeed_angle_deg>90?1:0} 1 ${rx} ${y}" stroke="#b98af044" stroke-width="2" fill="none"/><path d="M150 111 L${lx} ${y} M150 111 L${rx} ${y}" stroke="#b98af055" stroke-dasharray="3 4"/><path d="M150 111 V13" stroke="#ffffff14" stroke-dasharray="2 4"/><g fill="#201b2a" stroke="#b98af0"><rect x="${lx-9}" y="${y-12}" width="18" height="24" rx="4"/><rect x="${rx-9}" y="${y-12}" width="18" height="24" rx="4"/></g><g fill="none" stroke="#b98af0" opacity=".7"><circle cx="${lx}" cy="${y+3}" r="4"/><circle cx="${rx}" cy="${y+3}" r="4"/></g><text x="${lx}" y="${y-18}" fill="#b9bfc9" text-anchor="middle" font-size="11">L</text><text x="${rx}" y="${y-18}" fill="#b9bfc9" text-anchor="middle" font-size="11">R</text><circle cx="150" cy="111" r="13" fill="#24212d" stroke="#b98af0" stroke-width="1.5"/><path d="M146 101 L150 96 L154 101 M135 108 V116 M165 108 V116" fill="none" stroke="#b98af0" stroke-width="2"/><text x="150" y="73" text-anchor="middle" fill="#b98af0" font-size="12">±${settings.crossfeed_angle_deg}°</text>`;
  }
  function renderCross(preview) {
    if(!preview?.freq_hz?.length) return;
    const x=f=>25+Math.log10(f/20)/3*300, y=db=>clamp(70-db*2.2,4,84);
    let html='';
    [-12,0,12,24].forEach(db=>html+=`<line x1="25" x2="325" y1="${y(db)}" y2="${y(db)}" stroke="#ffffff0c"/><text x="19" y="${y(db)+3}" text-anchor="end" font-size="11" fill="#838997">${db}</text>`);
    [[preview.mid_db,'#b98af0'],[preview.side_db,'#7dd3c8'],[preview.ild_db,'#e8b34b']].forEach(([values,color])=>html+=`<path d="${pathFor(preview.freq_hz,values,x,y)}" fill="none" stroke="${color}" stroke-width="1.5"/>`);
    [20,1000,20000].forEach(f=>html+=`<text x="${x(f)}" y="98" text-anchor="middle" fill="#838997" font-size="11">${f>=1000?f/1000+'k':f}</text>`);
    $('cross-chart').innerHTML=html;
  }
  function drawLocal() { renderEQ(computePEQ(settings)); renderStage(); renderCross(computeCrossfeed(settings)); }
  function requestDraw() { if(!frame) frame=requestAnimationFrame(()=>{frame=null;drawLocal();}); }
  function renderMeters(meters) {
    if(!meters) return;
    state.meters=meters;
    if(Number.isFinite(meters.sample_rate) && meters.sample_rate > 0 &&
      (meters.sample_rate !== settings.sample_rate || (Number.isFinite(meters.chunksize) && meters.chunksize > 0 && meters.chunksize !== settings.chunksize))) {
      settings.sample_rate=meters.sample_rate;
      if(Number.isFinite(meters.chunksize) && meters.chunksize > 0) settings.chunksize=meters.chunksize;
      updateLabels(); requestDraw();
    }
    const peaks=meters.playback_peak||[-120,-120], rms=meters.playback_rms||peaks;
    $('output-meters').querySelectorAll('.stereo i').forEach((bar,i)=>{
      const db=meters.running ? (peaks[i]??-120) : -120;
      bar.querySelector('b').style.width=`${clamp((db+60)/60*100,0,100)}%`;
      bar.querySelector('em').style.left=`${clamp(((meters.running?rms[i]:-120)+60)/60*100,0,100)}%`;
      bar.title=`${i?'R':'L'} peak ${db.toFixed(1)} dBFS · RMS ${(rms[i]??-120).toFixed(1)} dBFS`;
    });
    const max=meters.running?Math.max(...peaks):-120;
    $('output-meters').setAttribute('aria-label',`Output peak ${max<=-100?'silent':max.toFixed(1)+' dBFS'}`);
    $('cpu').textContent=`${Number(meters.load_percent||0).toFixed(1)}%`;
    const clips=Number(meters.clipped_samples||0); if(clips<clipAcknowledged) clipAcknowledged=0;
    $('clip').classList.toggle('is-clipped',clips>clipAcknowledged); $('clip').querySelector('span').textContent=clips.toLocaleString();
    $('clip').title=`${clips} clipped samples. Click to acknowledge the warning.`;
    $('engine-status').textContent=meters.error?'● Engine error':meters.running?`● Running ${formatRate(settings.sample_rate)}`:`● ${meters.state||'Stopped'}`;
    $('engine-status').className=`status ${meters.error?'error':meters.running?'running':''}`;
    $('engine').innerHTML=meters.running?'Stop DSP <span>■</span>':'Start DSP <span>▶</span>';
    $('engine').disabled=!ready || engineBusy;
    if(meters.error) report(meters.error);
    if(meters.running && !pollTimer) startPolling();
    if(!meters.running && pollTimer) stopPolling();
  }
  function startPolling() {
    const generation=++pollGeneration;
    pollTimer=setInterval(async()=>{
      if(pollBusy || document.hidden || actionDepth) return;
      pollBusy=true;
      try { const meters=await invoke('get_live_meters'); if(generation===pollGeneration) renderMeters(meters); }
      catch(error) { if(generation===pollGeneration) report(error); }
      finally { pollBusy=false; }
    },200);
  }
  function stopPolling() { clearInterval(pollTimer); pollTimer=null; pollGeneration++; }
  function applyResponse(response,expectedRevision,full=false) {
    if(!response?.settings) throw Error('The audio engine returned an incomplete state.');
    // Never overwrite newer local edits with a slower backend response.
    if(expectedRevision!==revision) { renderMeters(response.meters); return; }
    state=response; settings=automaticPreamp(response.settings);
    if(full) renderSettings(); else { updateLabels(); syncAllBandRows(); }
    const automatic=response.settings.auto_preamp===true && response.settings.preamp_db===0 && JSON.stringify(response.settings.eq_bands)===JSON.stringify(settings.eq_bands);
    renderEQ((automatic && response.peq_preview)||computePEQ(settings)); renderStage(); renderCross(response.crossfeed_preview||computeCrossfeed(settings));
    renderMeters(response.meters);
    $('sync-status').textContent=previewMode?'Preview · changes simulated locally':'All changes applied';
  }
  function syncAllBandRows() { settings.eq_bands.forEach((_,i)=>syncBandRow(i)); }
  function edited({bands=false}={}) {
    if(!ready) return;
    revision++; $('dirty').hidden=false; updateLabels(); if(bands) renderBands();
    requestDraw(); $('sync-status').textContent=previewMode?'Updating preview…':'Applying changes…';
    // Coalesce the next 80 ms of edits without starving live audio during a
    // continuous drag. The last edit always gets its own trailing update.
    if(!updateTimer) updateTimer=setTimeout(sendUpdate,80);
  }
  function sendUpdate() {
    clearTimeout(updateTimer); updateTimer=null;
    const snapshot=clone(settings),expected=revision;
    return enqueue(async()=>{
      // A slow backend can accumulate work: only send the newest waiting edit.
      if(expected!==revision) return;
      actionDepth++;
      try { applyResponse(await invoke('update_dsp_settings',{settings:snapshot}),expected); }
      catch(error) { report(error); }
      finally { actionDepth--; }
    });
  }
  async function action(command,args={},full=true) {
    if(!ready) throw Error('Wait for the studio to finish connecting.');
    if(updateTimer) sendUpdate();
    return enqueue(async()=>{
      // Snapshot at execution time so any preceding queued update has settled.
      const expected=revision;
      actionDepth++;
      try {
        if(command==='start_engine') args={settings:clone(settings)};
        const response=await invoke(command,args);
        if(response.settings) applyResponse(response,expected,full);
        return response;
      } finally { actionDepth--; }
    });
  }

  // Global controls: local feedback first, ordered/debounced audio updates second.
  [['angle','crossfeed_angle_deg'],['mid','crossfeed_mid_boost_db']].forEach(([id,key])=>{
    $(id).addEventListener('input',event=>{settings[key]=Number(event.target.value);fillRange(event.target);edited();});
  });
  [['crossfeed-enabled','crossfeed_enabled'],['eq-enabled','eq_enabled'],['exclusive','exclusive_mode']].forEach(([id,key])=>$(id).addEventListener('change',event=>{settings[key]=event.target.checked;edited();}));
  $('bypass').addEventListener('click',()=>{settings.master_bypass=!settings.master_bypass;edited();});
  $('hw-vol').addEventListener('click',()=>{settings.hw_volume_bypass=!settings.hw_volume_bypass;edited();toast(settings.hw_volume_bypass?'Hardware volume mode: OS volume locked to 100% (0 dB).':'OS software volume control restored.');});
  $('angle-presets').addEventListener('click',event=>{const button=event.target.closest('[data-angle]');if(button){settings.crossfeed_angle_deg=Number(button.dataset.angle);setRange('angle',settings.crossfeed_angle_deg);edited();}});
  ['capture','playback'].forEach(direction=>$(direction).addEventListener('change',event=>{
    const device=(state[`${direction}_devices`]||[]).find(d=>d.id===event.target.value); if(!device) return;
    settings[`${direction}_device`]=device.id; settings[`${direction}_backend`]=device.backend;
    if(direction==='capture') settings.capture_loopback=device.is_loopback;
    edited();
  }));
  $('refresh').addEventListener('click',async()=>{
    $('refresh').disabled=true;
    try { await action('refresh_devices');toast('Audio devices refreshed.'); } catch(error) { report(error); } finally { $('refresh').disabled=false; }
  });
  $('engine').addEventListener('click',async()=>{
    if(engineBusy) return; engineBusy=true; $('engine').disabled=true;
    try { await action(state.meters?.running?'stop_engine':'start_engine'); if(previewMode) toast(state.meters.running?'Preview meters started. No audio is captured or played.':'Preview stopped.'); }
    catch(error) { report(error); }
    finally {engineBusy=false; $('engine').disabled=!ready;}
  });
  $('clip').addEventListener('click',()=>{clipAcknowledged=state?.meters?.clipped_samples||0; $('clip').classList.remove('is-clipped');});
  $('reset').addEventListener('click',()=>{
    settings.eq_bands=makeGeqBands();
    edited({bands:true});toast('Graphic EQ flattened to 0.0 dB.');
  });
  function setBandGain(index,value) {
    if(!ready || !settings.eq_bands[index] || !Number.isFinite(value)) return;
    selectedBand=index;settings.eq_bands[index].gain_db=Math.round(clamp(value,-geqMaxGain,geqMaxGain)*10)/10;
    syncAllBandRows();selectInfo(index);edited();
  }
  function bandInput(event) {
    const el=event.target,index=Number(el.dataset.band);
    if(!el.matches('.geq-num') || !settings.eq_bands[index]) return;
    const value=Number(el.value);
    if(el.value.trim()==='' || !Number.isFinite(value) || value < -geqMaxGain || value > geqMaxGain) {
      if(event.type==='change') el.value=settings.eq_bands[index].gain_db.toFixed(1);return;
    }
    setBandGain(index,value);
  }
  $('band-rows').addEventListener('input',bandInput);$('band-rows').addEventListener('change',bandInput);
  $('band-rows').addEventListener('focusin',event=>{const row=event.target.closest('[data-row]');if(row){selectedBand=Number(row.dataset.row);requestDraw();}});
  let faderDrag=null;
  function moveFader(event) {
    if(!faderDrag || event.pointerId!==faderDrag.pointerId) return;
    const rect=faderDrag.el.getBoundingClientRect();
    setBandGain(faderDrag.index,6-(event.clientY-rect.top)/rect.height*12);
  }
  $('band-rows').addEventListener('pointerdown',event=>{
    const el=event.target.closest('[data-fader]');if(!el || !ready || event.button!==0) return;
    event.preventDefault();el.focus();faderDrag={el,index:Number(el.dataset.fader),pointerId:event.pointerId};
    el.setPointerCapture(event.pointerId);moveFader(event);
  });
  $('band-rows').addEventListener('pointermove',moveFader);
  ['pointerup','pointercancel','lostpointercapture'].forEach(name=>$('band-rows').addEventListener(name,event=>{
    if(!faderDrag || faderDrag.pointerId!==event.pointerId) return;
    const {el,pointerId}=faderDrag;faderDrag=null;
    if(el.hasPointerCapture(pointerId)) el.releasePointerCapture(pointerId);
    if(updateTimer) sendUpdate();
  }));
  $('band-rows').addEventListener('dblclick',event=>{
    const el=event.target.closest('[data-fader]');if(el) setBandGain(Number(el.dataset.fader),0);
  });
  $('band-rows').addEventListener('wheel',event=>{
    const el=event.target.closest('[data-fader]');if(!el || event.ctrlKey || event.metaKey || !event.deltaY) return;
    event.preventDefault();const i=Number(el.dataset.fader);setBandGain(i,settings.eq_bands[i].gain_db+(event.deltaY<0?.5:-.5));
  },{passive:false});
  $('band-rows').addEventListener('keydown',event=>{
    const el=event.target.closest('[data-fader]');if(!el || event.ctrlKey || event.metaKey || event.altKey) return;
    const steps={ArrowUp:.5,ArrowDown:-.5,PageUp:2,PageDown:-2},i=Number(el.dataset.fader);
    if(Object.hasOwn(steps,event.key)) {event.preventDefault();setBandGain(i,settings.eq_bands[i].gain_db+steps[event.key]);}
    else if(['0','Delete','Backspace'].includes(event.key)) {event.preventDefault();setBandGain(i,0);}
  });

  // SVG drag math is independent of CSS pixel size, including non-uniform resizing.
  function graphPoint(event) {
    const rect=$('eq-graph').getBoundingClientRect();
    return {x:(event.clientX-rect.left)/rect.width*800,y:(event.clientY-rect.top)/rect.height*136};
  }
  function selectInfo(index) {
    const b=settings.eq_bands[index]; if(!b) return;
    const info=`${geqLabels[index]||`${b.freq_hz.toFixed(0)}`} Hz · ${signed(b.gain_db)} dB`;
    $('selected-info').textContent=info; $('graph-tooltip').textContent=info;
  }
  $('eq-graph').addEventListener('pointerdown',event=>{
    const handle=event.target.closest('[data-handle]'); if(!handle || !ready || event.button!==0) return;
    event.preventDefault();selectedBand=Number(handle.dataset.handle);drag={index:selectedBand,pointerId:event.pointerId};
    $('eq-graph').setPointerCapture(event.pointerId);$('graph-tooltip').hidden=false; selectInfo(selectedBand);requestDraw();
  });
  $('eq-graph').addEventListener('pointermove',event=>{
    if(!drag || drag.pointerId!==event.pointerId) return;
    const point=graphPoint(event),b=settings.eq_bands[drag.index]; if(!b) return;
    b.gain_db=Math.round(clamp(12-(point.y-graph.top)/(graph.bottom-graph.top)*24,-geqMaxGain,geqMaxGain)*10)/10;
    syncBandRow(drag.index);selectInfo(drag.index);edited();
  });
  function finishDrag(event) {
    if(!drag || (event && event.pointerId!==drag.pointerId)) return;
    const pointer=drag.pointerId;drag=null;$('graph-tooltip').hidden=true;
    if($('eq-graph').hasPointerCapture(pointer)) $('eq-graph').releasePointerCapture(pointer);
    if(updateTimer) sendUpdate();
  }
  ['pointerup','pointercancel','lostpointercapture'].forEach(name=>$('eq-graph').addEventListener(name,finishDrag));
  $('eq-graph').addEventListener('wheel',event=>{
    const handle=event.target.closest('[data-handle]');if(!handle || !ready || event.ctrlKey || event.metaKey || !event.deltaY)return;
    event.preventDefault();selectedBand=Number(handle.dataset.handle);const b=settings.eq_bands[selectedBand];
    b.gain_db=Math.round(clamp(b.gain_db+(event.deltaY<0?.5:-.5),-geqMaxGain,geqMaxGain)*10)/10;
    syncBandRow(selectedBand);selectInfo(selectedBand);edited();
  },{passive:false});
  $('eq-graph').addEventListener('dblclick',event=>{
    const handle=event.target.closest('[data-handle]');if(handle) setBandGain(Number(handle.dataset.handle),0);
  });

  // Native dialog elements provide keyboard focus trapping and Escape dismissal.
  document.querySelectorAll('[data-close]').forEach(button=>button.addEventListener('click',()=>button.closest('dialog').close()));
  document.querySelectorAll('dialog').forEach(dialog=>dialog.addEventListener('click',event=>{if(event.target===dialog){const r=dialog.getBoundingClientRect();if(event.clientX<r.left || event.clientX>r.right || event.clientY<r.top || event.clientY>r.bottom)dialog.close();}}));
  function openProfile(actionType) {
    profileAction=actionType;$('profile-error').textContent='';const deleting=actionType==='delete';
    $('profile-dialog-title').textContent=deleting?'Delete profile?':'Save listening profile';
    $('profile-description').textContent=deleting?'This removes the saved profile. Your current listening settings will stay unchanged.':'Keep your EQ and crossfeed settings together. An existing name will replace that saved profile.';
    $('profile-name').value=deleting?settings.active_profile:'';$('profile-name').readOnly=deleting;
    $('profile-submit').textContent=deleting?'Delete profile':'Save profile';
    $('profile-submit').style.background=deleting?'var(--danger)':'';$('profile-dialog').showModal();
    if(!deleting)$('profile-name').focus();
  }
  $('save-profile').addEventListener('click',()=>openProfile('save'));
  $('delete-profile').addEventListener('click',()=>openProfile('delete'));
  $('profile-form').addEventListener('submit',async event=>{
    event.preventDefault(); const name=$('profile-name').value.trim();if(!name){$('profile-error').textContent='Please enter a profile name.';return;}
    $('profile-submit').disabled=true;$('profile-error').textContent='';
    try {await action(profileAction==='delete'?'delete_profile':'save_profile',{name});$('dirty').hidden=true;$('profile-dialog').close();toast(profileAction==='delete'?'Profile deleted.':`Saved “${name}”.`);}
    catch(error){$('profile-error').textContent=error?.message||String(error);}
    finally{$('profile-submit').disabled=false;}
  });
  $('profiles').addEventListener('change',async event=>{
    const name=event.target.value;$('profiles').disabled=true;
    try {await action('load_profile',{name});selectedBand=-1;$('dirty').hidden=true;toast(`Loaded “${name}”.`);}
    catch(error){report(error);renderProfiles();}finally{$('profiles').disabled=false;}
  });
  $('autoeq').addEventListener('click',()=>{$('autoeq-error').textContent='';$('autoeq-dialog').showModal();});
  $('autoeq-search-link').addEventListener('click',event=>{
    if(previewMode) return;
    event.preventDefault();
    invoke('open_external_url', { url: 'https://autoeq.app' }).catch(report);
  });
  $('export').addEventListener('click',async()=>{
    $('export').disabled=true;
    try {const response=await action('export_autoeq',{},false);$('autoeq-text').value=response.text;$('autoeq-error').textContent='';toast('Current EQ exported. Ready to copy.');}
    catch(error){$('autoeq-error').textContent=error?.message||String(error);}finally{$('export').disabled=false;}
  });
  $('import').addEventListener('click',async()=>{
    const text=$('autoeq-text').value.trim();if(!text){$('autoeq-error').textContent='Paste an AutoEQ preset first.';return;}
    $('import').disabled=true;
    try {await action('import_autoeq',{text});selectedBand=-1;$('dirty').hidden=false;$('autoeq-dialog').close();toast('AutoEQ imported into 10-band Graphic EQ.');}
    catch(error){$('autoeq-error').textContent=error?.message||String(error);}finally{$('import').disabled=false;}
  });
  $('copy').addEventListener('click',async()=>{
    if(!$('autoeq-text').value.trim()){$('autoeq-error').textContent='Export your current EQ or paste a preset first.';return;}
    try {await navigator.clipboard.writeText($('autoeq-text').value);toast('Filter text copied to clipboard.');}
    catch{$('autoeq-text').focus();$('autoeq-text').select();$('autoeq-error').textContent='Clipboard is unavailable here. Text selected — press Ctrl+C (or ⌘C) to copy.';}
  });
  window.addEventListener('pagehide',()=>{stopPolling();clearTimeout(updateTimer);});
  window.addEventListener('pageshow',()=>{if(ready && state.meters?.running && !pollTimer)startPolling();});

  async function init() {
    let savedZoom = 1;
    try {
      const stored = localStorage.getItem(zoomStorageKey);
      if (stored !== null && stored.trim() !== '') savedZoom = Number(stored);
    } catch { /* Use 100% when storage is unavailable. */ }
    setUIZoom(savedZoom, false);
    $('preview-badge').hidden=!previewMode;
    if(previewMode) {
      $('footer-mode').textContent='BROWSER PREVIEW · SIMULATED METERS · NO AUDIO';
      $('cross-chart').setAttribute('aria-label','Approximate browser crossfeed response; not measured KU100 HRTF data');
      $('cross-chart').parentElement.title='Browser preview uses an analytical crossfeed approximation. Native mode displays the backend KU100 response.';
    }
    try {
      state=await invoke('get_app_state');ready=true;
      applyResponse(state,revision,true);$('app').setAttribute('aria-busy','false');$('engine').disabled=false;
    } catch(error) {
      $('app').setAttribute('aria-busy','false');$('engine-status').textContent='● Connection failed';$('engine-status').className='status error';
      report(error);$('sync-status').textContent='Audio engine unavailable. Restart the app to reconnect.';
      document.querySelectorAll('button,input,select').forEach(el=>el.disabled=true);
      // A native bridge failure must never silently look like a working mock engine.
    }
  }
  init();
})();
