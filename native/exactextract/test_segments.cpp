// Focused source-backing tests: no raster files or Python geospatial runtime.
// This test-only translation unit can inspect the private bridge raster types.
#include "bridge.cpp"
#include <array>
#include <cassert>
#include <iostream>
#include <map>

namespace {
struct Buffers {
    std::vector<double> values;
    std::vector<uint8_t> valid;
};
struct Fake {
    size_t calls = 0, releases = 0, fail_at = 0, malformed_at = 0, cancel_at = 0;
    bool cancelled = false;
    std::vector<std::array<size_t, 4>> reads;
    std::map<void*, std::unique_ptr<Buffers>> owned;
};
double sample(size_t x, size_t y, size_t band) { return double((3 * x + 7 * y + 13 * band) % 251) - 100.; }
bool valid(size_t x, size_t y, size_t band) { return (x + 3 * y + band) % 19 != 0; }
int32_t read_fake(void* ctx, size_t band, uint64_t x, uint64_t y, uint64_t w, uint64_t h,
                  SkarveEeWindow* out, char* error, size_t capacity) {
    auto& f = *static_cast<Fake*>(ctx);
    ++f.calls;
    f.reads.push_back({size_t(x), size_t(y), size_t(w), size_t(h)});
    if (f.calls == f.fail_at) { error_text(error, capacity, "injected second-segment failure"); return 1; }
    auto b = std::make_unique<Buffers>();
    for (size_t row = 0; row < h; ++row) for (size_t col = 0; col < w; ++col) {
        b->values.push_back(sample(x + col, y + row, band));
        b->valid.push_back(valid(x + col, y + row, band));
    }
    auto key = b.get();
    *out = {b->values.data(), b->valid.data(), size_t(w * h) - (f.calls == f.malformed_at), key};
    f.owned.emplace(key, std::move(b));
    if (f.calls == f.cancel_at) f.cancelled = true;
    return 0;
}
void release_fake(void* ctx, void* lease) {
    auto& f = *static_cast<Fake*>(ctx);
    assert(f.owned.erase(lease) == 1);
    ++f.releases;
}
int32_t cancel_fake(void* ctx) { return static_cast<Fake*>(ctx)->cancelled; }
SkarveEeGrid worldpop() {
    const double x=-5.60291663, y=15.173750141, dx=0.0008333333299732566, dy=0.0008333333299615327;
    return {x, y-7019*dy, x+9722*dx, y, dx, dy, 9722, 7019};
}
SkarveEeRequest request(Fake& f, const SkarveEeGrid& g, size_t max_cells=262144) {
    return {2, 1, &g, 1, nullptr, 0, max_cells, 268435456,
            &f, read_fake, release_fake, cancel_fake, 31, 0};
}
void check_raster(const RasterVariant& raster, const Grid<bounded_extent>& expected,
                  const Grid<bounded_extent>& full, size_t band=0) {
    std::visit([&](const auto& r) {
        assert(r->grid() == expected);
        const auto x=full.col_offset(expected), y=full.row_offset(expected);
        for (size_t row=0; row<r->rows(); ++row) for (size_t col=0; col<r->cols(); ++col) {
            const auto actual=(*r)(row,col);
            if (valid(x+col,y+row,band)) assert(actual == sample(x+col,y+row,band));
            else assert(std::isnan(actual));
        }
    }, raster);
}
void backing_tests() {
    const auto g=worldpop();
    const Grid<bounded_extent> full({g.xmin,g.ymin,g.xmax,g.ymax},g.dx,g.dy);
    const auto blocks=subdivide(full,262144);
    assert(blocks[113].rows()==26 && full.crop(blocks[113].extent()).rows()==27);
    for (size_t block : {size_t(110), size_t(113)}) {
        Fake fake;auto req=request(fake,g);SkarveEeMetrics metrics{};State state{req,metrics};
        {
            CallbackSource source(state,0,g);
            auto raster=source.read_box(blocks[block].extent());
            check_raster(raster,full.crop(blocks[block].extent()),full);
            assert(fake.calls==(block==110 ? 1U : 2U));
            for(const auto& w:fake.reads) assert(w[2]*w[3]<=req.max_cells);
            assert(state.live_bytes<=req.max_live_window_bytes);
            assert(metrics.read_calls==fake.calls);
            assert(metrics.read_cells==full.crop(blocks[block].extent()).size());
        }
        assert(fake.calls==fake.releases && fake.owned.empty() && state.live_bytes==0);
    }
    // A source grid may be wider than max_cells, including the admitted value1.
    {
        SkarveEeGrid small{0,0,4,3,1,1,4,3};Fake fake;auto req=request(fake,small,1);
        SkarveEeMetrics metrics{};State state{req,metrics};
        {
            CallbackSource source(state,0,small);
            auto raster=source.read_box(source.grid().extent());
            check_raster(raster,source.grid(),source.grid());
            assert(fake.calls==12);
            for(const auto& w:fake.reads) assert(w[2]==1 && w[3]==1);
        }
        assert(fake.calls==fake.releases && fake.owned.empty() && state.live_bytes==0);
    }
    // Even segmented metadata is charged before any source read/allocation.
    for (uint64_t budget : {uint64_t(1), uint64_t(262494*9)}) {
        Fake fake;auto req=request(fake,g);req.max_live_window_bytes=budget;
        SkarveEeMetrics metrics{};State state{req,metrics};CallbackSource source(state,0,g);
        try { source.read_box(blocks[113].extent()); assert(false); }
        catch(const Failure& e) { assert(e.status==4 && std::string(e.what()).find("aggregate live-window byte")!=std::string::npos); }
        assert(fake.calls==0 && fake.owned.empty() && state.live_bytes==0);
    }
    for (int fault : {0,1,2}) {
        Fake fake;if(fault==0) fake.fail_at=2;else if(fault==1) fake.malformed_at=2;else fake.cancel_at=2;
        auto req=request(fake,g);SkarveEeMetrics metrics{};State state{req,metrics};CallbackSource source(state,0,g);
        try { source.read_box(blocks[113].extent()); assert(false); }
        catch(const Failure& e) { assert(e.status==(fault==0 ? 3 : fault==1 ? 1 : 2)); }
        assert(fake.calls==2 && fake.releases==(fault==0 ? 1U : 2U));
        assert(fake.owned.empty() && state.live_bytes==0);
    }
    // Existing live rasters remain charged; a second cannot borrow their budget.
    {
        Fake fake;auto req=request(fake,g);req.max_live_window_bytes=3*1024*1024;
        SkarveEeMetrics metrics{};State state{req,metrics};
        {
            CallbackSource source(state,0,g);auto first=source.read_box(blocks[113].extent());
            try { source.read_box(blocks[113].extent()); assert(false); }
            catch(const Failure& e) { assert(e.status==4); }
            assert(fake.calls==2);check_raster(first,full.crop(blocks[113].extent()),full);
        }
        assert(fake.owned.empty() && state.live_bytes==0);
    }
}
class ProceduralRaster : public AbstractRaster<double> {
    size_t x_, y_;
public:
    ProceduralRaster(const Grid<bounded_extent>& sub, const Grid<bounded_extent>& full)
        : AbstractRaster<double>(sub), x_(full.col_offset(sub)), y_(full.row_offset(sub)) {}
    double operator()(size_t row, size_t col) const override {
        return valid(x_+col,y_+row,0) ? sample(x_+col,y_+row,0) : std::numeric_limits<double>::quiet_NaN();
    }
};
class NaturalSource : public RasterSource {
    Grid<bounded_extent> grid_;
public:
    explicit NaturalSource(const SkarveEeGrid& g) : grid_({g.xmin,g.ymin,g.xmax,g.ymax},g.dx,g.dy) { set_name("source0"); }
    const Grid<bounded_extent>& grid() const override { return grid_; }
    RasterVariant read_box(const Box& b) override { return std::make_unique<ProceduralRaster>(grid_.crop(b),grid_); }
};
std::vector<uint8_t> rectangle(double x0,double y0,double x1,double y1) {
    std::vector<uint8_t> b{1,3,0,0,0,1,0,0,0,5,0,0,0};
    for(auto xy : {std::array<double,2>{x0,y0},{x1,y0},{x1,y1},{x0,y1},{x0,y0}})
        for(double d:xy) { uint8_t bytes[8];std::memcpy(bytes,&d,8);b.insert(b.end(),bytes,bytes+8); }
    return b;
}
void upstream_answer_tests() {
    const auto g=worldpop();
    auto wkb=rectangle(g.xmin+4000.3*g.dx,g.ymax-2943.8*g.dy,g.xmin+4100.8*g.dx,g.ymax-2938.2*g.dy);
    SkarveEeWkb feature{wkb.data(),wkb.size()};
    for(uint32_t strategy:{0U,1U}) {
        Fake fake;auto req=request(fake,g);req.strategy=strategy;req.features=&feature;req.feature_count=1;
        double actual[5]{},expected[5]{};uint8_t defined[5]{},expected_defined[5]{};
        SkarveEeMetrics metrics{};char error[512]{};
        assert(skarve_ee_execute_v2(&req,actual,defined,5,&metrics,error,sizeof(error))==0);
        assert(fake.owned.empty() && fake.calls==fake.releases);
        if(strategy==1) assert(fake.calls==2);
        for(const auto& w:fake.reads) assert(w[2]*w[3]<=req.max_cells);
        // Independent source accessor, same unmodified upstream traversal/operations.
        SkarveEeMetrics natural_metrics{};State state{req,natural_metrics};
        InputFeatures features(state);NumericWriter writer(state,expected,expected_defined);
        NaturalSource source(g);std::unique_ptr<Processor> processor;
        if(strategy==0) processor=std::make_unique<FeatureSequentialProcessor>(features,writer);
        else processor=std::make_unique<RasterSequentialProcessor>(features,writer);
        const char* names[]={"sum","count","mean","min","max"};
        for(size_t i=0;i<5;++i) {auto op=Operation::create(names[i],std::to_string(i),&source);processor->add_operation(*op);}
        processor->set_max_cells_in_memory(req.max_cells);processor->set_grid_compat_tol(0);processor->process();writer.verify_complete();
        for(size_t i=0;i<5;++i) {assert(defined[i]==expected_defined[i]);assert(actual[i]==expected[i]);}
    }
}
}
int main() {
    backing_tests();upstream_answer_tests();
    std::cout << "{\"passed\":true,\"cases\":[\"normal_one_segment\",\"fractional_grid_overshoot\",\"max_cells_one_wide_grid\",\"payload_and_metadata_budget\",\"mid_segment_failure\",\"mid_segment_malformed\",\"mid_segment_cancellation\",\"aggregate_live_budget\",\"feature_and_raster_upstream_answers\"]}\n";
}
