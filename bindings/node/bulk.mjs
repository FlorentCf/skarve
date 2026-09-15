import koffi from 'koffi';
import { endianness } from 'node:os';

export const BULK_ABI_VERSION = 1;
export const BULK_LIMITS = Object.freeze({ bands: 64, windows: 4096, payloadBytes: 128 * 1024 * 1024,
  contributions: 268435456, globalOwnedBytes: 512 * 1024 * 1024 });
const policies = { strict_selected_v1: 1, hm_demographics_ordered_v1: 2 };
const reducers = { sum: 1, min: 2, max: 4, mean: 8 };
let ownedBytes = 0;
const align = n => Math.ceil(n / 8) * 8;
const integer = (n, name, max = Number.MAX_SAFE_INTEGER) => {
  if (!Number.isSafeInteger(n) || n < 0 || n > max) throw new RangeError(`${name} is out of range`);
  return n;
};
function checkedView(value, classes, name) {
  if (!classes.some(Type => value instanceof Type)) throw new TypeError(`${name} has an unsupported typed-array dtype`);
  const buffer = value.buffer;
  if (!(buffer instanceof ArrayBuffer) || buffer.resizable) throw new TypeError(`${name} requires a fixed, non-shared ArrayBuffer`);
  // Constructing a view also rejects detached zero-length buffers.
  return new Uint8Array(buffer, value.byteOffset, value.byteLength);
}

/** A private stable C allocation, with one dtype-preserving snapshot of every
 * supplied view. No caller pointer is sent to Koffi's asynchronous marshaller.
 * Only this module can reach the allocation; dispose after its callback.
 */
export function prepareBulk(windows, options = {}) {
  if (endianness() !== 'LE' || process.arch !== 'x64') throw new Error('Bulk ABI binding currently requires little-endian x86-64');
  const policy = options.policy ?? 'strict_selected_v1';
  if (!Object.hasOwn(policies, policy)) throw new RangeError('Unknown bulk numerical policy');
  const requested = options.reducers ?? ['sum'];
  if (!Array.isArray(requested) || !requested.length || new Set(requested).size !== requested.length || requested.some(x => !Object.hasOwn(reducers, x))) throw new RangeError('Unknown, duplicate or empty bulk reducers');
  const flags = requested.reduce((a, x) => a | reducers[x], 0);
  const maxPayload = integer(options.maxPayloadBytes ?? BULK_LIMITS.payloadBytes, 'maxPayloadBytes', BULK_LIMITS.payloadBytes);
  const maxContributions = integer(options.maxContributions ?? BULK_LIMITS.contributions, 'maxContributions', BULK_LIMITS.contributions);
  if (!maxPayload || !maxContributions) throw new RangeError('Bulk limits must be positive');
  if (!Array.isArray(windows) || !windows.length || windows.length > BULK_LIMITS.windows) throw new RangeError('Bulk requires 1–4096 windows');
  let descriptorCount = 0;
  for (const window of windows) {
    if (!Array.isArray(window.bands) || !window.bands.length || window.bands.length > BULK_LIMITS.bands) throw new RangeError('Each window requires 1–64 bands');
    descriptorCount += window.bands.length;
  }
  // Conservative bounded JS descriptor estimate is reserved as well as C bytes.
  const controlEstimate = (descriptorCount + windows.length) * 512;
  if (controlEstimate > 128 * 1024 * 1024) throw new RangeError('Bulk control metadata exceeds its bound');
  let payloadBytes = 0, selectionBytes = 0, paddedPayload = 0;
  const chunks = [];
  const add = (bytes, selection = false) => {
    payloadBytes += bytes.byteLength;
    if (payloadBytes > maxPayload) throw new RangeError('Bulk payload exceeds its byte limit');
    if (selection) selectionBytes += bytes.byteLength;
    const chunk = { bytes, offset: paddedPayload };
    paddedPayload += align(bytes.byteLength);
    chunks.push(chunk);
    return chunk;
  };
  const normalized = windows.map(window => {
    const bands = window.bands.map((band, position) => {
      const bytes = checkedView(band.values, [Float32Array, Float64Array], 'values');
      const size = band.values.BYTES_PER_ELEMENT;
      const byteOffset = integer(band.byteOffset ?? 0, 'byteOffset', bytes.byteLength);
      const byteStride = integer(band.byteStride ?? size, 'byteStride');
      if (byteOffset % size || byteStride < size || byteStride % size) throw new RangeError('Values offset and stride must be aligned to their dtype');
      const available = bytes.byteLength - byteOffset;
      const cellCount = integer(band.cellCount ?? (available < size ? 0 : Math.floor((available - size) / byteStride) + 1), 'cellCount');
      if (cellCount && ((cellCount - 1) * byteStride + size > available || !Number.isSafeInteger((cellCount - 1) * byteStride + size))) throw new RangeError('Values selection exceeds view bounds');
      const id = integer(band.id ?? position, 'band id', 0xffffffff);
      const hasNodata = Object.hasOwn(band, 'nodata');
      if (hasNodata && typeof band.nodata !== 'number') throw new TypeError('nodata must be a number');
      let validity = null, validityKind = 0, validityOffset = 0;
      if (band.validity !== undefined) {
        validityKind = band.validityKind === 'bits' ? 2 : band.validityKind === undefined || band.validityKind === 'bytes' ? 1 : -1;
        if (validityKind < 0) throw new RangeError('Unknown validityKind');
        validityOffset = integer(band.validityOffset ?? 0, 'validityOffset');
        const view = checkedView(band.validity, [Uint8Array], 'validity');
        if (validityOffset + cellCount > view.byteLength * (validityKind === 2 ? 8 : 1)) throw new RangeError('Validity selection exceeds view bounds');
        validity = add(view);
      } else if (band.validityKind !== undefined || band.validityOffset !== undefined) throw new TypeError('Validity options require a validity array');
      return { data: add(bytes), dtype: size === 4 ? 1 : 2, byteOffset, byteStride, cellCount, id,
        hasNodata, nodata: band.nodata ?? 0, validity, validityKind, validityOffset };
    });
    let selection = null, selectionKind = 0, selectionCount = 0;
    if (window.selection !== undefined) {
      const value = window.selection;
      if (value && Object.hasOwn(value, 'spans')) {
        const view = checkedView(value.spans, [BigUint64Array], 'spans');
        if (value.spans.length % 2) throw new RangeError('Spans require interleaved start,count pairs');
        selection = add(view, true); selectionKind = 3; selectionCount = value.spans.length / 2;
      } else {
        const view = checkedView(value, [Uint32Array, BigUint64Array], 'selection');
        selection = add(view, true); selectionKind = value instanceof Uint32Array ? 1 : 2; selectionCount = value.length;
      }
    }
    return { bands, selection, selectionKind, selectionCount };
  });
  const bandCount = normalized[0].bands.length;
  const windowOffset = 56, bandOffset = windowOffset + windows.length * 48;
  const resultOffset = bandOffset + descriptorCount * 88, metaOffset = resultOffset + bandCount * 80;
  const errorOffset = metaOffset + 72, dataOffset = align(errorOffset + 512);
  const allocationBytes = dataOffset + paddedPayload;
  const reservation = allocationBytes + controlEstimate;
  if (reservation > maxPayload) throw new RangeError('Bulk payload plus descriptor/owner state exceeds per-call budget');
  if (ownedBytes + reservation > BULK_LIMITS.globalOwnedBytes) throw new RangeError('Global bulk snapshot byte budget exceeded');
  ownedBytes += reservation;
  let pointer;
  try {
    pointer = koffi.alloc('uint8_t', allocationBytes);
    if (!pointer) throw new Error('Bulk snapshot allocation failed');
    const base = koffi.address(pointer);
    const memory = Buffer.from(koffi.view(pointer, allocationBytes));
    memory.fill(0, 0, dataOffset);
    const u64 = (offset, value) => memory.writeBigUInt64LE(BigInt(value), offset);
    const ptr = (offset, target) => u64(offset, base + BigInt(target));
    for (const chunk of chunks) memory.set(chunk.bytes, dataOffset + chunk.offset);
    memory.writeUInt32LE(1, 0); memory.writeUInt32LE(56, 4); memory.writeUInt32LE(policies[policy], 8); memory.writeUInt32LE(flags, 12);
    ptr(16, windowOffset); u64(24, windows.length); u64(32, maxPayload); u64(40, maxContributions);
    let nextBand = bandOffset;
    normalized.forEach((window, wi) => {
      const wo = windowOffset + wi * 48;
      ptr(wo, nextBand); memory.writeUInt32LE(window.bands.length, wo + 8);
      if (window.selection) { ptr(wo + 16, dataOffset + window.selection.offset); u64(wo + 24, window.selection.bytes.byteLength); }
      u64(wo + 32, window.selectionCount); memory.writeUInt32LE(window.selectionKind, wo + 40);
      for (const band of window.bands) {
        const bo = nextBand; nextBand += 88;
        ptr(bo, dataOffset + band.data.offset); u64(bo + 8, band.data.bytes.byteLength); u64(bo + 16, band.byteOffset);
        u64(bo + 24, band.byteStride); u64(bo + 32, band.cellCount);
        if (band.validity) { ptr(bo + 40, dataOffset + band.validity.offset); u64(bo + 48, band.validity.bytes.byteLength); }
        u64(bo + 56, band.validityOffset); memory.writeDoubleLE(band.nodata, bo + 64);
        memory.writeUInt32LE(band.id, bo + 72); memory.writeUInt32LE(band.dtype, bo + 76);
        memory.writeUInt32LE(band.validityKind, bo + 80); memory.writeUInt32LE(band.hasNodata ? 1 : 0, bo + 84);
      }
    });
    let disposed = false;
    return {
      arguments: [base, base + BigInt(resultOffset), bandCount, base + BigInt(metaOffset), base + BigInt(errorOffset), 512],
      payloadBytes, allocationBytes, reservation,
      decode(status) {
        if (disposed) throw new Error('Bulk snapshot already released');
        if (status !== 0) {
          const end = memory.indexOf(0, errorOffset);
          const error = new Error(memory.toString('utf8', errorOffset, Math.min(end < 0 ? errorOffset + 512 : end, errorOffset + 512)) || `Native bulk status ${status}`);
          error.code = ({ 1: 'BULK_INVALID', 2: 'BUSY', 3: 'ABORTED', 4: 'BULK_NONFINITE', 5: 'BULK_PANIC' })[status] ?? 'BULK_ERROR';
          throw error;
        }
        const bands = [];
        for (let i = 0; i < bandCount; i++) {
          const o = resultOffset + i * 80, hasValues = !!(memory.readUInt32LE(o + 4) & 1);
          const band = { id: memory.readUInt32LE(o), has_values: hasValues };
          for (const [name, offset] of [['sum',8],['min',16],['max',24],['mean',32]]) if (flags & reducers[name]) band[name] = name === 'sum' || hasValues ? memory.readDoubleLE(o + offset) : null;
          for (const [name, offset] of [['valid_count',40],['excluded_mask',48],['excluded_nodata',56],['excluded_nonfinite',64],['excluded_negative',72]]) band[name] = Number(memory.readBigUInt64LE(o + offset));
          bands.push(band);
        }
        const metadata = { abi_version: memory.readUInt32LE(metaOffset), policy, selection: 'caller_supplied_ordered_multiset', source_identity: 'caller_asserted_ephemeral', ownership: 'owned_dtype_preserving_snapshot',
          copied_bytes: payloadBytes, binding_allocation_bytes: allocationBytes, binding_reserved_bytes: reservation, control_estimate_bytes: controlEstimate };
        for (const [name, offset] of [['window_count',8],['band_count',16],['payload_bytes',24],['selection_bytes',32],['result_bytes',40],['native_owned_bytes',48],['validation_ns',56],['reduction_ns',64]]) metadata[name] = Number(memory.readBigUInt64LE(metaOffset + offset));
        return { bands, metadata };
      },
      dispose() { if (!disposed) { disposed = true; koffi.free(pointer); ownedBytes -= reservation; } },
    };
  } catch (error) { if (pointer) koffi.free(pointer); ownedBytes -= reservation; throw error; }
}

export function bulkMemoryStats() { return { owned_bytes: ownedBytes, limit_bytes: BULK_LIMITS.globalOwnedBytes }; }
