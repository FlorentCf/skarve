/** Compile-only declaration contract; see the runtime product API tests. */
import Skarve, {Engine, RasterEngine, type Geometry, type BackendProvenance} from '../bindings/node/index';

async function productContract(zone:Geometry) {
  const sk:Engine = new Skarve();
  const compatibility:RasterEngine = sk;
  const source = await compatibility.infuse('generated.tif');
  const result = await source.carve({zone,bands:[0],metrics:['sum','support'],backend:'exactextract',
    numerical_policy:'exactextract_fractional_v030',execution_envelope:'embedded_cooperative',
    backend_options:{strategy:'feature-sequential',window_bytes:1024*1024}});
  const provenance:BackendProvenance|undefined = result.provenance;
  if(provenance) console.log(provenance.selected_backend,provenance.upstream_version);
  const job={zones:[{id:'z',version:'1',geometry:zone}],slices:[{id:'s',spec:{location:'generated.tif'}}],
    crs:'EPSG:3857',metrics:['sum' as const,'support' as const],backend:'native' as const};
  for await(const page of sk.cleave(job,{maxRows:2})) {
    if(page.descriptor?.schema==='skarve_exactextract_five_v1') console.log(page.descriptor.slices);
    if(page.descriptor?.schema==='skarve_numeric_six_v1') console.log(page.descriptor.slice_id);
  }
  await source.ward('new-index',{boundary_source:'original'});
  await source.compile('generated.skv',{chunk_edge:64,band_group:4,codec:'deflate',predictor:'byte_delta_v1',summaries:true,signal:new AbortController().signal});
  await sk.verifySkv('generated.skv');
  const rawSkv = await sk.infuse({location:'generated.skv',format:'skv',use_summaries:false});
  await rawSkv.close();
  // @ts-expect-error Compilation accepts only the documented chunk edges.
  source.compile('bad.skv',{chunk_edge:17});
  // @ts-expect-error Predictors must name a supported reversible representation.
  source.compile('bad.skv',{predictor:'approximate'});
  // @ts-expect-error Backend names are explicit, not arbitrary strings.
  source.carve({zone,backend:'misspelled'});
  // @ts-expect-error A legacy resident algorithm is not a source execution backend.
  source.carve({zone,backend:'row_blocks'});
  // @ts-expect-error Bands are indices, not names.
  source.carve({zone,bands:['temperature']});
  await sk.close();
}

void productContract;
