/// The mass render's profile is allocator-dominated (malloc/realloc/memmove
/// churn from parser Vec growth and include splicing); jemalloc's
/// thread-cached bins cut it where glibc's arena binning loses.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> anyhow::Result<()> {
    kolorinko::main()
}
