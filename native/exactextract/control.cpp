// Independent direct-upstream control. This executable is development evidence,
// not the installed engine interface. Upstream C++ is the same linked archive.
#include "skarve_exactextract.h"
#include "feature_sequential_processor.h"
#include "map_feature.h"
#include "raster_sequential_processor.h"

#include <chrono>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <limits>

using namespace exactextract;
using Clock = std::chrono::steady_clock;
using Values = std::vector<double>;
uint64_t ns(Clock::time_point start) { return std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - start).count(); }
template<class T> T read_one(std::istream& stream) {
    T value;
    stream.read(reinterpret_cast<char*>(&value), sizeof(value));
    if (!stream) throw std::runtime_error("truncated control job");
    return value;
}
struct Job {
    uint64_t strategy, width, height, bands, zones, max_cells;
    SkarveEeGrid grid;
    std::vector<Values> values;
    std::vector<std::vector<uint8_t>> masks, geometries;
    explicit Job(const char* path) {
        std::ifstream in(path, std::ios::binary);
        char magic[8]; in.read(magic, 8);
        if (!in || std::string(magic, 8) != "SKAREE01") throw std::runtime_error("bad control job magic");
        strategy = read_one<uint64_t>(in); width = read_one<uint64_t>(in); height = read_one<uint64_t>(in);
        bands = read_one<uint64_t>(in); zones = read_one<uint64_t>(in); max_cells = read_one<uint64_t>(in);
        if (strategy > 1 || !width || !height || width > 1048576 || height > 1048576 || width * height > 16777216 || !bands || bands > 64 || !zones || zones > 65536 || !max_cells)
            throw std::runtime_error("control job admission exceeded");
        grid = {};
        grid.xmin = read_one<double>(in); grid.ymin = read_one<double>(in); grid.xmax = read_one<double>(in); grid.ymax = read_one<double>(in);
        grid.dx = read_one<double>(in); grid.dy = read_one<double>(in); grid.width = width; grid.height = height;
        if (width * height * bands > 134217728) throw std::runtime_error("control arrays exceed 1152 MiB");
        for (size_t b = 0; b < bands; ++b) {
            values.emplace_back(width * height);
            masks.emplace_back(width * height);
            in.read(reinterpret_cast<char*>(values.back().data()), values.back().size() * sizeof(double));
            in.read(reinterpret_cast<char*>(masks.back().data()), masks.back().size());
            if (!in) throw std::runtime_error("truncated control raster");
        }
        uint64_t geometry_bytes = 0;
        for (size_t z = 0; z < zones; ++z) {
            const auto size = read_one<uint64_t>(in);
            if (size > 67108864 - geometry_bytes) throw std::runtime_error("control geometry cap");
            geometry_bytes += size;
            geometries.emplace_back(size);
            in.read(reinterpret_cast<char*>(geometries.back().data()), size);
            if (!in) throw std::runtime_error("truncated control geometry");
        }
        if (in.peek() != std::char_traits<char>::eof()) throw std::runtime_error("trailing control job bytes");
    }
};
class ArrayRaster final : public AbstractRaster<double> {
    const Values& values_;
    const std::vector<uint8_t>& mask_;
public:
    ArrayRaster(const Grid<bounded_extent>& grid, const Values& values, const std::vector<uint8_t>& mask)
        : AbstractRaster<double>(grid), values_(values), mask_(mask) {}
    double operator()(size_t r, size_t c) const override {
        const auto i = r * cols() + c;
        return mask_[i] ? values_[i] : std::numeric_limits<double>::quiet_NaN();
    }
};
class ArraySource final : public RasterSource {
    Grid<bounded_extent> grid_;
    ArrayRaster raster_;
public:
    ArraySource(const Job& job, size_t band)
        : grid_({job.grid.xmin, job.grid.ymin, job.grid.xmax, job.grid.ymax}, job.grid.dx, job.grid.dy), raster_(grid_, job.values[band], job.masks[band]) {
        set_name("source" + std::to_string(band));
    }
    const Grid<bounded_extent>& grid() const override { return grid_; }
    RasterVariant read_box(const Box& box) override { return std::make_unique<RasterView<double>>(raster_, grid_.crop(box)); }
};
class Geometries final : public FeatureSource {
    std::vector<MapFeature> values_;
    size_t index_ = 0;
public:
    Geometries(const Job& job, GEOSContextHandle_t context) {
        auto reader = GEOSWKBReader_create_r(context);
        for (const auto& bytes : job.geometries) {
            auto geometry = geos_ptr(context, GEOSWKBReader_read_r(context, reader, bytes.data(), bytes.size()));
            if (!geometry) { GEOSWKBReader_destroy_r(context, reader); throw std::runtime_error("invalid control WKB"); }
            values_.emplace_back(); values_.back().set_geometry(std::move(geometry));
        }
        GEOSWKBReader_destroy_r(context, reader);
    }
    size_t count() const override { return values_.size(); }
    bool next() override { return index_ < values_.size() ? (++index_, true) : false; }
    const Feature& feature() const override { return values_.at(index_ - 1); }
};
class MapWriter final : public OutputWriter {
public:
    std::vector<MapFeature> rows;
    void write(const Feature& feature) override { rows.emplace_back(feature); }
};
struct Result {
    Values values;
    std::vector<uint8_t> defined;
    uint64_t upstream_ns = 0, complete_ns = 0;
    SkarveEeMetrics metrics{};
};
Result natural(const Job& job) {
    const auto started = Clock::now();
    Result result;
    result.values.resize(job.zones * job.bands * 5);
    result.defined.resize(result.values.size());
    const auto context = GEOS_init_r();
    {
        Geometries features(job, context);
        MapWriter writer;
        std::vector<std::unique_ptr<ArraySource>> sources;
        for (size_t b = 0; b < job.bands; ++b) sources.push_back(std::make_unique<ArraySource>(job, b));
        std::unique_ptr<Processor> processor;
        if (job.strategy == 0) processor = std::make_unique<FeatureSequentialProcessor>(features, writer);
        else processor = std::make_unique<RasterSequentialProcessor>(features, writer);
        const char* stats[] = {"sum", "count", "mean", "min", "max"};
        for (size_t b = 0; b < job.bands; ++b) for (size_t s = 0; s < 5; ++s) {
            auto op = Operation::create(stats[s], std::to_string(b * 5 + s), sources[b].get());
            processor->add_operation(*op);
        }
        processor->set_max_cells_in_memory(job.max_cells);
        const auto upstream_started = Clock::now();
        processor->process();
        result.upstream_ns = ns(upstream_started);
        if (writer.rows.size() != job.zones) throw std::runtime_error("incomplete direct result");
        for (size_t z = 0; z < job.zones; ++z) for (size_t b = 0; b < job.bands; ++b) {
            const auto& fields = writer.rows[z].map();
            const auto count = fields.find(std::to_string(b * 5 + 1));
            const auto support = count == fields.end() ? 0.0 : std::get<double>(count->second);
            if (!std::isfinite(support) || support < 0) throw std::runtime_error("invalid direct support");
            if (count == fields.end()) {
                for (size_t s = 0; s < 5; ++s)
                    if (fields.count(std::to_string(b * 5 + s))) throw std::runtime_error("direct result missing support for populated band");
            }
            for (size_t s = 0; s < 5; ++s) {
                const auto index = (z * job.bands + b) * 5 + s;
                // The natural writer may omit optional extrema for an empty
                // intersection. Stage the declared empty result without asking
                // MapFeature for fields upstream did not assign.
                if (support == 0) {
                    result.values[index] = 0;
                    result.defined[index] = s < 2 ? 1 : 0;
                    continue;
                }
                const auto value = writer.rows[z].get_double(std::to_string(b * 5 + s));
                if (std::isfinite(value)) { result.values[index] = value; result.defined[index] = 1; }
                else throw std::runtime_error("nonfinite positive-support direct result");
            }
        }
    }
    GEOS_finish_r(context);
    result.complete_ns = ns(started);
    return result;
}
struct Window { Values values; std::vector<uint8_t> mask; };
int32_t read_window(void* ctx, size_t band, uint64_t x, uint64_t y, uint64_t w, uint64_t h, SkarveEeWindow* out, char*, size_t) {
    try {
        const auto& job = *static_cast<Job*>(ctx);
        auto window = std::make_unique<Window>();
        window->values.reserve(w * h); window->mask.reserve(w * h);
        for (size_t row = y; row < y + h; ++row) for (size_t col = x; col < x + w; ++col) {
            window->values.push_back(job.values[band][row * job.width + col]);
            window->mask.push_back(job.masks[band][row * job.width + col]);
        }
        *out = {window->values.data(), window->mask.data(), window->values.size(), window.get()};
        window.release();
        return 0;
    } catch (...) { return 1; }
}
void release_window(void*, void* window) { delete static_cast<Window*>(window); }
Result bridged(Job& job) {
    const auto started = Clock::now();
    Result result;
    result.values.resize(job.zones * job.bands * 5); result.defined.resize(result.values.size());
    std::vector<SkarveEeGrid> sources(job.bands, job.grid);
    std::vector<SkarveEeWkb> features;
    for (const auto& geometry : job.geometries) features.push_back({geometry.data(), geometry.size()});
    SkarveEeRequest request{2, static_cast<uint32_t>(job.strategy), sources.data(), sources.size(), features.data(), features.size(),
        job.max_cells, 1073741824, &job, read_window, release_window, nullptr, 31, 0};
    char error[1024];
    const auto code = skarve_ee_execute_v2(&request, result.values.data(), result.defined.data(), result.values.size(), &result.metrics, error, sizeof(error));
    if (code) throw std::runtime_error(error);
    result.upstream_ns = result.metrics.upstream_nanoseconds;
    result.complete_ns = ns(started);
    return result;
}
int main(int argc, char** argv) {
    try {
        if (argc != 4) throw std::runtime_error("usage: skarve-ee-control upstream|bridge JOB.bin REPEATS");
        const std::string mode(argv[1]);
        if (mode != "upstream" && mode != "bridge") throw std::runtime_error("mode must be upstream or bridge");
        const auto loading = Clock::now();
        Job job(argv[2]);
        const auto load_ns = ns(loading);
        const int repeats = std::stoi(argv[3]);
        if (repeats < 1 || repeats > 100) throw std::runtime_error("repeat admission exceeded");
        std::cout << std::setprecision(17) << "{\"schema\":1,\"mode\":\"" << mode << "\",\"upstream_version\":\"0.3.0\",\"geos_version\":\"" << GEOSversion()
                  << "\",\"source_snapshot_load_ns\":" << load_ns << ",\"runs\":[";
        for (int r = 0; r < repeats; ++r) {
            auto result = mode == "upstream" ? natural(job) : bridged(job);
            if (r) std::cout << ',';
            std::cout << "{\"complete_ns\":" << result.complete_ns << ",\"upstream_ns\":" << result.upstream_ns
                << ",\"callback_ns\":" << result.metrics.callback_nanoseconds << ",\"read_calls\":" << result.metrics.read_calls
                << ",\"read_bytes\":" << result.metrics.read_bytes << ",\"values\":[";
            for (size_t i = 0; i < result.values.size(); ++i) { if (i) std::cout << ','; std::cout << result.values[i]; }
            std::cout << "],\"defined\":[";
            for (size_t i = 0; i < result.defined.size(); ++i) { if (i) std::cout << ','; std::cout << static_cast<int>(result.defined[i]); }
            std::cout << "]}";
        }
        std::cout << "]}\n";
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
