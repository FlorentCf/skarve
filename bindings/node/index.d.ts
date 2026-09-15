export type Position = [number, number];
export type Geometry = { type: 'Polygon'; coordinates: Position[][] } | { type: 'MultiPolygon'; coordinates: Position[][][] };
export interface Grid { width: number; height: number; transform: [number,number,number,number,number,number]; crs: string }
export interface Band { values: (number|null)[]; valid?: boolean[]; nodata?: number|null; scale?: number; offset?: number; unit?: string }
export interface Raster { grid: Grid; bands: Band[] }
export interface CallOptions { signal?: AbortSignal }
export type BulkPolicy = 'strict_selected_v1'|'hm_demographics_ordered_v1';
export interface BulkBand { id?:number; values:Float32Array|Float64Array; nodata?:number; validity?:Uint8Array; validityKind?:'bytes'|'bits'; validityOffset?:number; byteOffset?:number; byteStride?:number; cellCount?:number }
export interface BulkWindow { bands:BulkBand[]; selection?:Uint32Array|BigUint64Array|{spans:BigUint64Array} }
export interface BulkOptions extends CallOptions { policy?:BulkPolicy; reducers?:('sum'|'min'|'max'|'mean')[]; maxPayloadBytes?:number; maxContributions?:number }
export interface BulkBandResult { id:number; has_values:boolean; sum?:number; min?:number|null; max?:number|null; mean?:number|null; valid_count:number; excluded_mask:number; excluded_nodata:number; excluded_nonfinite:number; excluded_negative:number }
export interface BulkResult { bands:BulkBandResult[]; metadata:{abi_version:number;policy:BulkPolicy;copied_bytes:number;binding_allocation_bytes:number;binding_reserved_bytes:number;window_count:number;band_count:number;payload_bytes:number;selection_bytes:number;result_bytes:number;native_owned_bytes:number;validation_ns:number;reduction_ns:number;binding_timing_ms:Record<string,number>;[key:string]:unknown} }
export const BULK_ABI_VERSION: 1;
export const BULK_LIMITS: Readonly<{bands:number;windows:number;payloadBytes:number;contributions:number;globalOwnedBytes:number}>;
export function bulkMemoryStats():{owned_bytes:number;limit_bytes:number};
export type Statistic = 'sum'|'support'|'mean'|'min'|'max'|'count'|'histogram'|'weighted_sum'|'weight_sum'|'weighted_mean'|'variance'|'stddev'|'weighted_variance'|'weighted_stddev'|'categories'|'majority'|'variety'|'median'|'quantiles'|'weighted_median'|'weighted_quantiles';
export type ExecutionBackend = 'native'|'exactextract'|'auto';
export type Backend = ExecutionBackend|'direct'|'row_blocks'|'hierarchy'|'cumulative_full'|'cumulative_blocked'|'persistent_flat'|'persistent_hierarchy';
export type NumericalPolicy = 'native_grid_planar_fractional'|'exactextract_fractional_v030'|'exactextract_rasterio_v030';
export interface BackendSelection {
  backend?:ExecutionBackend;
  numerical_policy?:NumericalPolicy;
  accepted_policies?:NumericalPolicy[];
  execution_envelope?:'embedded_cooperative'|'process_isolated';
  backend_options?:{strategy?:'raster-sequential'|'feature-sequential';max_cells_in_memory?:number;window_bytes?:number;output_bytes?:number;max_windows?:number;decoded_bytes?:number};
}
export interface BackendProvenance {selected_backend:'native'|'exactextract';requested_backend:'native'|'exactextract'|'auto';numerical_policy:NumericalPolicy;upstream_version?:string|null;source_interpretation?:string;[key:string]:unknown}
export interface MeasureOptions extends CallOptions, Omit<BackendSelection,'backend'> { crs?: string; bands?: number[]; strategy?: 'direct'|'scanline'; backend?:Backend; statistics?: Statistic[]; histogram_edges?: number[]; weight_band?: number; category_values?:number[]; quantiles?:number[]; quantile_max_samples?:number }
/** Zone coordinates must already use the source CRS; omitted crs asserts that CRS. */
export interface CarveOptions extends Omit<MeasureOptions,'backend'> { zone:Geometry; metrics?:Statistic[];backend?:ExecutionBackend }
export interface SourceSpec { location:string; format?:'geotiff'|'netcdf'|'skv'; variable?:string; overview?:number; bands?:number[]; use_summaries?:boolean; crs?:string; longitude_shift?:-360|0|360; identity?:{sha256:string;byte_length:number;policy:'verify'|'trusted_manifest';etag?:string}; http?:{headers?:Record<string,string>;header_env?:Record<string,string>;max_requests?:number;max_download_bytes?:number;max_range_bytes?:number;timeout_seconds?:number;allow_http?:boolean;cache_bytes?:number} }
/** Experimental SKV v0 compilation. Bounds are validated by the native engine. */
export interface CompileOptions extends CallOptions { chunk_edge?:64|128|256;band_group?:number;codec?:'deflate'|'none';predictor?:'none'|'byte_delta_v1';payload_layout?:'band'|'row_group_v1';compression_level?:number;summaries?:boolean;working_bytes?:number;max_output_bytes?:number }
/** Exactly ordered center-mask selections; run ends are exclusive. No geometry or overview inference. */
export type OrderedSourceWindow = {window:[number,number,number,number]} & ({indexes:number[];runs?:never}|{runs:[number,number][];indexes?:never});
export interface OrderedSourceRequest { polygons:{id:string;windows:OrderedSourceWindow[]}[];bands?:number[];nodata?:(number|null)[];budget?:{working_bytes?:number;planning_bytes?:number;max_read_calls?:number;max_contributions?:number;read_materialized_bytes?:number} }
export interface OrderedSourceOptions extends CallOptions {numerical_policy:'hm_demographics_ordered_v1'}
export interface OrderedSourceResult { rows:{id:string;bands:BulkBandResult[]}[];complete:boolean;numerical_policy:'hm_demographics_ordered_v1';metrics:Record<string,unknown>;provenance:Record<string,unknown> }
/** Exact IEEE metadata bits use 16 lowercase hex digits, never lossy JS integers. */
export interface ServingView {
  grid:{width:number;height:number;transform_f64_bits:string[];crs:string};
  raw_metadata:{bands:{scalar_type:string;nodata_f64_bits:string|null;scale_f64_bits:string;offset_f64_bits:string;unit:string|null;mask_flags:number;original_band_index:number;description:string}[];pixel_convention:string;source_band_count:number;source_overview?:number|null};
}
export interface ServingObjectAttestation {sha256:string;byte_length:number;selector_sha256:string;expected_view_sha256:string}
/** Provision only from a trusted complete typed-sample, mask and interpretation comparison. */
export interface ServingProfile {
  schema:'skarve_serving_profile_v1';profile_id:string;view_id:string;numerical_policy:'hm_demographics_ordered_v1';
  direct:{spec:SourceSpec;expected:ServingView};accelerated:{spec:SourceSpec;expected:ServingView};
  attestation:{kind:'owner_verified_full_view_v1';verifier:string;receipt_sha256:string;checks:('typed_sample_bits'|'mask_bytes'|'full_interpretation')[];samples_sha256:string;masks_sha256:string;interpretation_sha256:string;cells_per_band:number;direct:ServingObjectAttestation;accelerated:ServingObjectAttestation};
  accelerated_when:{enabled:boolean;access_classes:string[];bands:number[];polygons:[number,number];windows:[number,number];selected_cells:[number,number];envelope_width:[number,number];envelope_height:[number,number];envelope_cells:[number,number]};
}
export interface OrderedProfileOptions extends OrderedSourceOptions {view_id:string;access_class?:string}
export interface OrderedProfileResult extends OrderedSourceResult {routing:Record<string,unknown>}
export type Expression = {op:'band';band:number}|{op:'constant';value:number}|{op:'valid';input:Expression}|{op:'add'|'subtract'|'multiply'|'divide'|'normalized_difference'|'greater'|'greater_equal'|'less'|'and';left:Expression;right:Expression};
export interface BatchJob extends BackendSelection { zones:{id:string;version:string;geometry:Geometry}[];slices:{id:string;time?:string;variable?:string;source?:string;spec?:SourceSpec;bands?:number[];index?:string;expected_build_id?:string}[];crs:string;options?:Omit<MeasureOptions,keyof BackendSelection|'signal'|'crs'|'strategy'>;expression?:Expression;mask?:Expression;schedule?:'feature'|'tile'|'mixed';geometry_layout?:'auto'|'compact'|'compact_shared'|'compact_rows'|'csr';tile_edge?:32|64|128|256|512;window_policy?:'fixed'|'source_layout';output_mode?:'full'|'numeric';budget?:{working_bytes?:number;geometry_bytes?:number;tile_bytes?:number;output_bytes?:number;max_windows?:number;decoded_bytes?:number;max_contributions?:number;workers?:1} }
export interface BatchRow {result_id:string;zone_id:string;zone_version:string;slice_id:string;time?:string|null;variable?:string|null;source_id?:string;grid_id?:string;mode?:string;bands:BandResult[];provenance?:BackendProvenance}
export interface BatchCheckpoint {version:1;fingerprint:string;next_row:number;pins:({source_id:string;grid_id:string;index_build_id:string|null}|null)[]}
export interface NativeNumericDescriptor {schema:'skarve_numeric_six_v1';mode:string;fingerprint:string;source_id:string;grid_id:string;slice_id:string;time:string|null;variable:string|null;statistics:Statistic[];bands:{band:number;unit:string|null}[]}
export interface ExactextractNumericDescriptor {schema:'skarve_exactextract_five_v1';statistics:Statistic[];provenance:BackendProvenance;slices:{slice_id:string;time:string|null;variable:string|null;source_id:string;grid_id:string;bands:number[]}[]}
export interface BatchPage {rows:BatchRow[];complete:boolean;checkpoint?:BatchCheckpoint|null;checkpoint_supported?:boolean;metrics:Record<string,unknown>;descriptor?:NativeNumericDescriptor|ExactextractNumericDescriptor;buffered_rows?:number;buffered_output_bytes?:number;provenance?:BackendProvenance}
export interface BatchOptions extends CallOptions {id?:string;maxRows?:number;checkpoint?:BatchCheckpoint;checkpointInterval?:number}
export type CleaveJob = BatchJob & {metrics?:Statistic[]};
export interface PrepareOptions extends CallOptions { backend?: 'row_blocks'|'hierarchy'|'cumulative_full'|'cumulative_blocked'; tile_edge?: number; columns?: number; bands?: number[] }
export interface JointPlannerOptions {mode?:'joint'|'greedy'|'fixed_request_first';model:{request_latency_ns:number;bandwidth_bytes_per_second:number};limits?:{max_states?:number;beam_width?:number;max_planner_bytes?:number;max_total_read_bytes?:number;max_requests?:number;max_range_transitions?:number};max_range_bytes?:number;max_summary_gap_bytes?:number;summary_reduction_ns?:number;raw_cell_reduction_ns?:number}
export interface FileMeasureOptions extends MeasureOptions { crs:string; path?:string; index?:string; raw_path?:string; expected_build_id?:string; read_memory_bytes?:number; summary_page_bytes?:0|4096|16384|65536; coalesce_raw?:boolean; forbidden_raw_tiles?:number[];joint_planner?:JointPlannerOptions }
export interface BandResult {
  fractional_sum?: number; covered_cell_equivalents?: number; selected_cell_equivalents?: number;
  missing_cell_equivalents?: number; outside_cell_equivalents?: number; intersecting_cell_count?: number; valid_cell_count?: number;
  coverage_weighted_mean?: number|null; min?: number|null; max?: number|null; status?: string; unit?: string|null;
  weighted_sum?: number; weight_sum?: number; weighted_mean?: number|null; histogram?: unknown;
  variance?:number|null;stddev?:number|null;weighted_variance?:number|null;weighted_stddev?:number|null;
  categories?:unknown;majority?:number|null;variety?:number;median?:number|null;weighted_median?:number|null;
  quantiles?:{probabilities:number[];values:(number|null)[]};weighted_quantiles?:{probabilities:number[];values:(number|null)[]};
  moment_diagnostics?:{rational_fallback?:boolean;weighted_rational_fallback?:boolean};
}
export interface MeasureResult { bands: BandResult[]; grid: Grid; source_id: string; mode: string; strategy?: string; timing_ms: Record<string,number>; plan?: unknown;provenance?:BackendProvenance }
export class RasterEngineError extends Error { code: string }
export class RasterEngine {
  constructor(options?: { library?: string });
  readonly busy: boolean; readonly closed: boolean;
  readonly lastTiming: Record<string,unknown> | null;
  bulkReduce(windows:BulkWindow[],options?:BulkOptions):Promise<BulkResult>;
  request(request: Record<string,unknown>, options?: CallOptions): Promise<any>;
  open(id: string, rasterOrPath: Raster|string, options?: CallOptions): Promise<any>;
  compile(source: string, id: string, geometry: Geometry, crs: string, options?: CallOptions & { strategy?: 'direct'|'scanline'; debug_cells?: boolean }): Promise<any>;
  measure(source: string, geometryOrPlan: Geometry|string, options?: MeasureOptions): Promise<MeasureResult>;
  prepare(source: string, options?: PrepareOptions): Promise<any>;
  prepareFile(path:string,index:string,options?:CallOptions & {tile_edge?:16|64|256;layout?:'band_major'|'cell_major'|'band_groups_4';summary_backend?:'flat'|'hierarchy'}):Promise<any>;
  measureFile(geometry:Geometry,options:FileMeasureOptions):Promise<MeasureResult>;
  registerFile(id: string, path: string, options?: CallOptions): Promise<any>;
  registerSource(id:string,spec:SourceSpec,options?:CallOptions):Promise<any>;
  sourceInfo(source:string,options?:CallOptions):Promise<any>;
  openSource(spec:SourceSpec|string,options?:CallOptions & {id?:string}):Promise<Source>;
  infuse(source:SourceSpec|string,options?:CallOptions & {id?:string}):Promise<Source>;
  compileSource(source:string,output:string,options?:CompileOptions):Promise<Record<string,unknown>>;
  verifySkv(source:SourceSpec|string,options?:CallOptions):Promise<Record<string,unknown>>;
  closeReader(source:string,options?:CallOptions):Promise<any>;
  measureSource(source:string,geometry:Geometry,options:MeasureOptions & {crs:string;index?:string;index_handle?:string;expected_build_id?:string;read_memory_bytes?:number;forbidden_raw_tiles?:number[];joint_planner?:JointPlannerOptions}):Promise<MeasureResult>;
  sumSelectedSource(source:string,request:OrderedSourceRequest,options:OrderedSourceOptions):Promise<OrderedSourceResult>;
  sumSelected(profile:ServingProfile,request:OrderedSourceRequest,options:OrderedProfileOptions):Promise<OrderedProfileResult>;
  registerIndex(source:string,id:string,index:string,options?:CallOptions & {expected_build_id?:string;read_memory_bytes?:number}):Promise<any>;
  openIndex(source:string,index:string,options?:CallOptions & {id?:string;expected_build_id?:string;read_memory_bytes?:number}):Promise<Index>;
  indexInfo(id:string,options?:CallOptions):Promise<any>;
  closeIndex(id:string,options?:CallOptions):Promise<any>;
  measureHMPopulation(source:string,geometry:Geometry,options?:CallOptions & {index?:string;expected_build_id?:string;read_memory_bytes?:number}):Promise<any>;
  prepareSource(source:string,index:string,options?:CallOptions & {tile_edge?:16|64|256;layout?:'band_major'|'cell_major'|'band_groups_4';summary_backend?:'flat'|'hierarchy';boundary_source?:'original'|'normalized'}):Promise<any>;
  batchPages(job:BatchJob,options?:BatchOptions):AsyncGenerator<BatchPage>;
  cleave(job:CleaveJob,options?:BatchOptions):AsyncGenerator<BatchPage>;
  batch(job:BatchJob,options?:BatchOptions):AsyncGenerator<BatchRow>;
  configureFileCache(bytes: number, options?: CallOptions): Promise<any>;
  measureRegisteredFile(source: string, geometry: Geometry, options: Omit<MeasureOptions,'strategy'> & {crs:string}): Promise<MeasureResult>;
  clearFileCache(options?: CallOptions): Promise<any>;
  closeFile(source: string, options?: CallOptions): Promise<any>;
  closeSource(source: string, options?: CallOptions): Promise<any>;
  stats(options?: CallOptions): Promise<any>;
  cancel(): void;
  close(): Promise<void>;
}

export class Source {
  readonly id:string; readonly metadata:any; readonly closed:boolean;
  inspect(options?:CallOptions):Promise<any>;
  measure(geometry:Geometry,options:MeasureOptions & {crs:string;index?:string;expected_build_id?:string;read_memory_bytes?:number}):Promise<MeasureResult>;
  sumSelected(request:OrderedSourceRequest,options:OrderedSourceOptions):Promise<OrderedSourceResult>;
  carve(options:CarveOptions & {index?:string;index_handle?:string;expected_build_id?:string;read_memory_bytes?:number}):Promise<MeasureResult>;
  openIndex(index:string,options?:CallOptions & {id?:string;expected_build_id?:string;read_memory_bytes?:number}):Promise<Index>;
  prepare(index:string,options?:CallOptions & {tile_edge?:16|64|256;layout?:'band_major'|'cell_major'|'band_groups_4';summary_backend?:'flat'|'hierarchy';boundary_source?:'original'|'normalized'}):Promise<any>;
  ward(index:string,options?:CallOptions & {tile_edge?:16|64|256;layout?:'band_major'|'cell_major'|'band_groups_4';summary_backend?:'flat'|'hierarchy';boundary_source?:'original'|'normalized'}):Promise<any>;
  compile(output:string,options?:CompileOptions):Promise<Record<string,unknown>>;
  close():Promise<void>;
}
export { RasterEngine as Skarve, RasterEngine as Engine, RasterEngineError as SkarveError };
export default RasterEngine;

export class Index {
  readonly id:string; readonly source:string; readonly metadata:any; readonly closed:boolean;
  inspect(options?:CallOptions):Promise<any>;
  measure(geometry:Geometry,options:MeasureOptions & {crs:string}):Promise<MeasureResult>;
  close():Promise<void>;
}
