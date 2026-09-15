#include "skarve_exactextract.h"

#include "feature_sequential_processor.h"
#include "map_feature.h"
#include "raster_sequential_processor.h"

#include <algorithm>
#include <charconv>
#include <chrono>
#include <cmath>
#include <cstring>
#include <limits>
#include <mutex>
#include <thread>

namespace {
using Clock = std::chrono::steady_clock;
using namespace exactextract;
std::mutex execution_mutex;
thread_local bool inside_execute = false;

struct Failure : std::runtime_error {
    int32_t status;
    Failure(int32_t code, const std::string& message) : std::runtime_error(message), status(code) {}
};
uint64_t elapsed(Clock::time_point start) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - start).count();
}
void error_text(char* out, size_t capacity, const char* message) noexcept {
    if (out && capacity) {
        const auto n = std::min(capacity - 1, std::strlen(message));
        std::memcpy(out, message, n);
        out[n] = '\0';
    }
}
size_t checked_sum(size_t a, size_t b) {
    if (b > std::numeric_limits<size_t>::max() - a) throw Failure(4, "exactextract size overflow");
    return a + b;
}
size_t checked_product(size_t a, size_t b) {
    if (b && a > std::numeric_limits<size_t>::max() / b) throw Failure(4, "exactextract size overflow");
    return a * b;
}
struct State {
    const SkarveEeRequest& request;
    SkarveEeMetrics& metrics;
    uint64_t live_bytes = 0;
    void poll() const {
        if (request.cancelled && request.cancelled(request.context)) throw Failure(2, "exactextract cooperatively cancelled");
    }
};
struct Lease {
    State& state;
    SkarveEeWindow window;
    uint64_t bytes;
    Lease(State& s, SkarveEeWindow w, uint64_t n) : state(s), window(w), bytes(n) {
        state.live_bytes += bytes;
        state.metrics.peak_live_window_bytes = std::max(state.metrics.peak_live_window_bytes, state.live_bytes);
    }
    ~Lease() {
        if (window.lease) state.request.release(state.request.context, window.lease);
        state.live_bytes -= bytes;
    }
};
class BorrowedRaster final : public AbstractRaster<double> {
    std::unique_ptr<Lease> lease_;
public:
    BorrowedRaster(const Grid<bounded_extent>& grid, std::unique_ptr<Lease> lease)
        : AbstractRaster<double>(grid), lease_(std::move(lease)) {}
    double operator()(size_t row, size_t col) const override {
        const auto offset = row * cols() + col;
        const auto& window = lease_->window;
        return window.valid && !window.valid[offset]
            ? std::numeric_limits<double>::quiet_NaN() : window.values[offset];
    }
};
// Keep the upstream-cropped grid intact even when floating-point re-cropping
// adds a boundary row/column beyond max_cells. Only callback storage is split;
// coverage grids, source sample order, and upstream subdivision are unchanged.
class SegmentedRaster final : public AbstractRaster<double> {
    State& state_;
    size_t segment_cols_, segment_rows_, col_segments_, metadata_bytes_;
    std::vector<std::unique_ptr<Lease>> leases_;
public:
    SegmentedRaster(const Grid<bounded_extent>& grid, State& state,
                    size_t cols, size_t rows, size_t count, size_t metadata_bytes)
        : AbstractRaster<double>(grid), state_(state), segment_cols_(cols),
          segment_rows_(rows), col_segments_((grid.cols() - 1) / cols + 1),
          metadata_bytes_(metadata_bytes) {
        // Reserve before charging: a throwing constructor must leave State intact.
        leases_.reserve(count);
        state_.live_bytes += metadata_bytes_;
        state_.metrics.peak_live_window_bytes =
            std::max(state_.metrics.peak_live_window_bytes, state_.live_bytes);
    }
    ~SegmentedRaster() override { state_.live_bytes -= metadata_bytes_; }
    void add(std::unique_ptr<Lease> lease) { leases_.push_back(std::move(lease)); }
    double operator()(size_t row, size_t col) const override {
        const auto segment_col = col / segment_cols_;
        const auto index = (row / segment_rows_) * col_segments_ + segment_col;
        const auto width = std::min(segment_cols_, cols() - segment_col * segment_cols_);
        const auto offset = (row % segment_rows_) * width + col % segment_cols_;
        const auto& window = leases_[index]->window;
        return window.valid && !window.valid[offset]
            ? std::numeric_limits<double>::quiet_NaN() : window.values[offset];
    }
};
class CallbackSource final : public RasterSource {
    State& state_;
    size_t index_;
    Grid<bounded_extent> grid_;

    std::unique_ptr<Lease> read_segment(size_t x, size_t y, size_t width, size_t height) {
        state_.poll();
        const auto cells = checked_product(width, height);
        if (cells > state_.request.max_cells)
            throw Failure(4, "exactextract callback segment exceeds max_cells: " +
                std::to_string(cells) + " > " + std::to_string(state_.request.max_cells));
        const auto reserved_bytes = checked_product(cells, 9);
        const auto available = state_.request.max_live_window_bytes - state_.live_bytes;
        if (reserved_bytes > available)
            throw Failure(4, "exactextract aggregate live-window byte budget exceeded: segment " +
                std::to_string(reserved_bytes) + ", available " + std::to_string(available));
        SkarveEeWindow window{};
        char callback_error[512]{};
        const auto start = Clock::now();
        const int32_t status = state_.request.read(state_.request.context, index_, x, y,
            width, height, &window, callback_error, sizeof(callback_error));
        state_.metrics.callback_nanoseconds += elapsed(start);
        state_.metrics.read_calls++;
        callback_error[sizeof(callback_error) - 1] = '\0';
        if (status) throw Failure(3, callback_error[0] ? callback_error : "exactextract source callback failed");
        // Successful callbacks transfer exactly one lease even if malformed.
        // Establish its owner before any validation can throw.
        std::unique_ptr<Lease> lease;
        try { lease = std::make_unique<Lease>(state_, window, reserved_bytes); }
        catch (...) {
            if (window.lease) state_.request.release(state_.request.context, window.lease);
            throw;
        }
        if (!window.lease || !window.values || window.length != cells)
            throw Failure(1, "exactextract source callback returned a malformed lease");
        state_.metrics.read_cells += cells;
        state_.metrics.read_bytes += checked_product(cells, window.valid ? 9 : 8);
        // Validate once per window, never by reentering a source per pixel.
        for (size_t i = 0; i < cells; ++i) {
            if (window.valid && window.valid[i] > 1) throw Failure(1, "exactextract source validity must be zero or one");
            if ((!window.valid || window.valid[i]) && !std::isfinite(window.values[i]))
                throw Failure(1, "exactextract normalized valid source value must be finite");
        }
        state_.poll();
        return lease;
    }
public:
    CallbackSource(State& state, size_t index, const SkarveEeGrid& grid)
        : state_(state), index_(index), grid_({grid.xmin, grid.ymin, grid.xmax, grid.ymax}, grid.dx, grid.dy) {
        if (grid_.rows() != grid.height || grid_.cols() != grid.width) throw Failure(1, "exactextract grid dimensions disagree with extent/resolution");
        set_name("source" + std::to_string(index));
    }
    const Grid<bounded_extent>& grid() const override { return grid_; }
    RasterVariant read_box(const Box& box) override {
        state_.poll();
        auto subgrid = grid_.crop(box);
        if (subgrid.empty()) return std::make_unique<Raster<double>>(Grid<bounded_extent>::make_empty());
        const auto cells = checked_product(subgrid.rows(), subgrid.cols());
        const auto x0 = grid_.col_offset(subgrid);
        const auto y0 = grid_.row_offset(subgrid);
        if (x0 > grid_.cols() || subgrid.cols() > grid_.cols() - x0 || y0 > grid_.rows() || subgrid.rows() > grid_.rows() - y0)
            throw Failure(1, "exactextract requested a source window outside its grid");
        if (cells <= state_.request.max_cells)
            return std::make_unique<BorrowedRaster>(subgrid,
                read_segment(x0, y0, subgrid.cols(), subgrid.rows()));
        const auto columns = std::min<size_t>(subgrid.cols(), state_.request.max_cells);
        const auto rows = static_cast<size_t>(state_.request.max_cells) / columns;
        const auto count = checked_product((subgrid.cols() - 1) / columns + 1,
                                           (subgrid.rows() - 1) / rows + 1);
        // Charge all segment owners/pointers and the wrapper against the same
        // aggregate window limit, before allocation or the first callback.
        const auto metadata_bytes = checked_sum(sizeof(SegmentedRaster),
            checked_product(count, sizeof(Lease) + sizeof(std::unique_ptr<Lease>)));
        const auto total = checked_sum(checked_product(cells, 9), metadata_bytes);
        const auto available = state_.request.max_live_window_bytes - state_.live_bytes;
        if (total > available)
            throw Failure(4, "exactextract aggregate live-window byte budget exceeded: segmented raster " +
                std::to_string(total) + ", available " + std::to_string(available));
        auto raster = std::make_unique<SegmentedRaster>(subgrid, state_, columns, rows, count, metadata_bytes);
        for (size_t row = 0; row < subgrid.rows(); row += rows) {
            for (size_t col = 0; col < subgrid.cols(); col += columns) {
                raster->add(read_segment(x0 + col, y0 + row,
                    std::min(columns, subgrid.cols() - col), std::min(rows, subgrid.rows() - row)));
            }
        }
        return raster;
    }
};
struct GeosContext {
    GEOSContextHandle_t handle = GEOS_init_r();
    GeosContext() { if (!handle) throw Failure(1, "GEOS context allocation failed"); }
    ~GeosContext() { GEOS_finish_r(handle); }
};
class InputFeatures final : public FeatureSource {
    State& state_;
    GeosContext context_;
    std::vector<MapFeature> features_;
    size_t next_ = 0;
public:
    explicit InputFeatures(State& state) : state_(state) {
        const auto reader = GEOSWKBReader_create_r(context_.handle);
        if (!reader) throw Failure(1, "GEOS WKB reader allocation failed");
        const auto destroy_reader = [this](GEOSWKBReader* p) { GEOSWKBReader_destroy_r(context_.handle, p); };
        std::unique_ptr<GEOSWKBReader, decltype(destroy_reader)> owned_reader(reader, destroy_reader);
        features_.reserve(state.request.feature_count);
        for (size_t i = 0; i < state.request.feature_count; ++i) {
            state_.poll();
            const auto& wkb = state.request.features[i];
            if (!wkb.data || !wkb.length) throw Failure(1, "exactextract requires nonempty WKB");
            auto geometry = geos_ptr(context_.handle, GEOSWKBReader_read_r(context_.handle, reader, wkb.data, wkb.length));
            if (!geometry) throw Failure(1, "exactextract could not parse WKB geometry");
            const auto type = GEOSGeomTypeId_r(context_.handle, geometry.get());
            if ((type != GEOS_POLYGON && type != GEOS_MULTIPOLYGON) || GEOSisEmpty_r(context_.handle, geometry.get()) != 0 || GEOSisValid_r(context_.handle, geometry.get()) != 1)
                throw Failure(1, "exactextract requires valid nonempty Polygon or MultiPolygon geometry");
            features_.emplace_back();
            features_.back().set_geometry(std::move(geometry));
        }
    }
    bool next() override { state_.poll(); return next_ < features_.size() ? (++next_, true) : false; }
    const Feature& feature() const override { return features_.at(next_ - 1); }
    size_t count() const override { return features_.size(); }
};
// Upstream requires a Feature interface, but every numeric setter writes to the
// final caller-staged row. No result property map or second numeric row exists.
class NumericFeature final : public MapFeature {
    double* values_;
    uint8_t* defined_;
    size_t columns_;
    uint32_t fields_;
public:
    NumericFeature(double* values, uint8_t* defined, size_t columns, uint32_t fields)
        : values_(values), defined_(defined), columns_(columns), fields_(fields) {}
    using MapFeature::set;
    void set(const std::string& name, double value) override {
        size_t column = 0;
        const auto parsed = std::from_chars(name.data(), name.data() + name.size(), column);
        if (parsed.ec != std::errc() || parsed.ptr != name.data() + name.size() || column >= columns_)
            throw Failure(1, "exactextract returned an unexpected output field");
        if (!(fields_ & (1U << (column % 5))))
            throw Failure(1, "exactextract returned an unrequested output field");
        if (std::isinf(value)) throw Failure(1, "exactextract result overflowed binary64");
        values_[column] = std::isnan(value) ? 0.0 : value;
        defined_[column] = std::isnan(value) ? 0 : 1;
    }
    void set(const std::string& name, int32_t value) override { set(name, static_cast<double>(value)); }
    void set(const std::string& name, int64_t value) override { set(name, static_cast<double>(value)); }
};
class NumericWriter final : public OutputWriter {
    State& state_;
    double* values_;
    uint8_t* defined_;
    size_t columns_;
    size_t rows_ = 0;
public:
    NumericWriter(State& state, double* values, uint8_t* defined)
        : state_(state), values_(values), defined_(defined), columns_(checked_product(state.request.source_count, 5)) {}
    std::unique_ptr<Feature> create_feature() override {
        state_.poll();
        if (rows_ >= state_.request.feature_count) throw Failure(1, "exactextract produced too many feature rows");
        return std::make_unique<NumericFeature>(values_ + rows_ * columns_, defined_ + rows_ * columns_, columns_, state_.request.statistics_mask | 2U);
    }
    void write(const Feature&) override {
        state_.poll();
        for (size_t i = 0; i < columns_; i += 5) {
            const auto offset = rows_ * columns_ + i;
            if (!defined_[offset + 1] || values_[offset + 1] < 0)
                throw Failure(1, "exactextract returned undefined fractional support");
            if ((state_.request.statistics_mask & 1U) && !defined_[offset])
                throw Failure(1, "exactextract returned undefined requested sum");
            for (size_t field = 2; field < 5; ++field) {
                if (!(state_.request.statistics_mask & (1U << field))) continue;
                if (values_[offset + 1] == 0) { values_[offset + field] = 0; defined_[offset + field] = 0; }
                else if (!defined_[offset + field]) throw Failure(1, "exactextract returned undefined positive-support statistics");
            }
        }
        ++rows_;
    }
    void verify_complete() const {
        if (rows_ != state_.request.feature_count) throw Failure(1, "exactextract produced an incomplete answer");
    }
};
void validate(const SkarveEeRequest& request, size_t output_length) {
    if (request.abi_version != SKARVE_EE_ABI_VERSION || request.strategy > 1 || !request.read || !request.release
        || !request.statistics_mask || (request.statistics_mask & ~31U) || request.reserved)
        throw Failure(1, "invalid exactextract ABI request");
    if (!request.sources || !request.features || !request.source_count || !request.feature_count || !request.max_cells || !request.max_live_window_bytes)
        throw Failure(1, "exactextract requires nonempty bounded sources, features and budgets");
    if (request.source_count > 256 || request.feature_count > 65536 || request.max_cells > 16777216)
        throw Failure(4, "exactextract input exceeds bridge admission limits");
    if (output_length != checked_product(checked_product(request.source_count, request.feature_count), 5))
        throw Failure(1, "exactextract output capacity does not match request");
    size_t wkb_bytes = 0;
    for (size_t i = 0; i < request.feature_count; ++i) {
        const auto length = request.features[i].length;
        if (length > 64 * 1024 * 1024 - wkb_bytes) throw Failure(4, "exactextract WKB budget exceeded");
        wkb_bytes += length;
    }
    const auto& first = request.sources[0];
    for (size_t i = 0; i < request.source_count; ++i) {
        const auto& grid = request.sources[i];
        if (!std::isfinite(grid.xmin) || !std::isfinite(grid.ymin) || !std::isfinite(grid.xmax) || !std::isfinite(grid.ymax) || !std::isfinite(grid.dx) || !std::isfinite(grid.dy)
            || grid.dx <= 0 || grid.dy <= 0 || grid.xmin >= grid.xmax || grid.ymin >= grid.ymax || !grid.width || !grid.height
            || grid.width > 2147483647 || grid.height > 2147483647)
            throw Failure(1, "exactextract requires finite positive north-up grids");
        const auto cols = (grid.xmax - grid.xmin) / grid.dx;
        const auto rows = (grid.ymax - grid.ymin) / grid.dy;
        if (!std::isfinite(cols) || !std::isfinite(rows) || std::round(cols) != grid.width || std::round(rows) != grid.height)
            throw Failure(1, "exactextract grid extent overflow");
        if (checked_product(grid.width, grid.height) / request.max_cells > 1000000)
            throw Failure(4, "exactextract subdivision count exceeds admission limit");
        if (grid.xmin != first.xmin || grid.ymin != first.ymin || grid.xmax != first.xmax || grid.ymax != first.ymax || grid.dx != first.dx || grid.dy != first.dy || grid.width != first.width || grid.height != first.height)
            throw Failure(1, "exactextract embedded backend requires exactly matching grids; no resampling");
    }
}
} // namespace

extern "C" int32_t skarve_ee_execute_v2(const SkarveEeRequest* request, double* values, uint8_t* defined,
    size_t output_length, SkarveEeMetrics* metrics, char* error, size_t error_capacity) {
    const auto started = Clock::now();
    if (metrics) *metrics = {};
    error_text(error, error_capacity, "");
    int32_t status = 0;
    try {
        if (!request || !values || !defined || !metrics) throw Failure(1, "null exactextract request or output");
        if (inside_execute) throw Failure(1, "reentrant exactextract execution is unsupported");
        validate(*request, output_length);
        State state{*request, *metrics};
        std::unique_lock<std::mutex> guard(execution_mutex, std::defer_lock);
        while (!guard.try_lock()) { state.poll(); std::this_thread::sleep_for(std::chrono::milliseconds(2)); }
        inside_execute = true;
        struct ReentrancyGuard { ~ReentrancyGuard() { inside_execute = false; } } reentrancy_guard;
        state.poll();
        std::fill(values, values + output_length, 0);
        std::fill(defined, defined + output_length, 0);
        InputFeatures features(state);
        NumericWriter writer(state, values, defined);
        std::vector<std::unique_ptr<CallbackSource>> sources;
        for (size_t i = 0; i < request->source_count; ++i) sources.push_back(std::make_unique<CallbackSource>(state, i, request->sources[i]));
        std::unique_ptr<Processor> processor;
        if (request->strategy == 0) processor = std::make_unique<FeatureSequentialProcessor>(features, writer);
        else processor = std::make_unique<RasterSequentialProcessor>(features, writer);
        const char* statistics[] = {"sum", "count", "mean", "min", "max"};
        for (size_t band = 0; band < sources.size(); ++band) {
            for (size_t field = 0; field < 5; ++field) {
                if (!((request->statistics_mask | 2U) & (1U << field))) continue;
                const auto operation = Operation::create(statistics[field], std::to_string(band * 5 + field), sources[band].get());
                processor->add_operation(*operation);
            }
        }
        processor->set_max_cells_in_memory(request->max_cells);
        processor->set_grid_compat_tol(0);
        processor->show_progress(true);
        processor->set_progress_fn([&state](double, std::string_view) { state.poll(); });
        const auto upstream_started = Clock::now();
        try { processor->process(); }
        catch (...) { metrics->upstream_nanoseconds = elapsed(upstream_started); throw; }
        metrics->upstream_nanoseconds = elapsed(upstream_started);
        writer.verify_complete();
        state.poll();
    } catch (const Failure& failure) {
        status = failure.status;
        error_text(error, error_capacity, failure.what());
    } catch (const std::bad_alloc&) {
        status = 4;
        error_text(error, error_capacity, "exactextract allocation failed; no partial answer is valid");
    } catch (const std::exception& failure) {
        status = 1;
        error_text(error, error_capacity, failure.what());
    } catch (...) {
        status = 1;
        error_text(error, error_capacity, "unknown exactextract C++ exception");
    }
    if (metrics) metrics->total_nanoseconds = elapsed(started);
    return status;
}
extern "C" const char* skarve_ee_upstream_version() { return "0.3.0"; }
extern "C" const char* skarve_ee_upstream_commit() { return "94f5882ad6904d9d44d9199164d671fa48dc78eb"; }
extern "C" const char* skarve_ee_geos_version() { return GEOSversion(); }
