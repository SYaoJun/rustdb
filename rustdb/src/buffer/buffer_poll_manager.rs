use crate::buffer::lru_k_replacer::LruKReplacer;
use crate::buffer::FrameId;
use crate::error::{RustDBError, RustDBResult};
use crate::storage::codec::{Decoder, Encoder};
use crate::storage::disk::disk_manager::DiskManager;
use crate::storage::page::b_plus_tree::Node;
use crate::storage::page::Page;
use crate::storage::{PageId};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

//fixme will be dead lock, need to be fixed
pub struct BufferPoolManager {
    inner: RwLock<Inner>,
    disk_manager: DiskManager,
    next_page_id: AtomicUsize,
    pool_size: usize,
}

struct Inner {
    pages: Vec<Arc<RwLock<Page>>>,
    replacer: Arc<RwLock<LruKReplacer>>,
    page_table: HashMap<PageId, FrameId>,
    free_list: VecDeque<FrameId>,
}

impl BufferPoolManager {
    pub async fn new(pool_size: usize, k: usize, disk_manager: DiskManager) -> RustDBResult<Self> {
        let replacer = Arc::new(RwLock::new(LruKReplacer::new(pool_size, k)));
        let mut free_list = VecDeque::with_capacity(pool_size);
        for frame_id in 0..pool_size {
            free_list.push_back(frame_id as FrameId);
        }
        let pages = {
            let mut v = Vec::with_capacity(pool_size);
            (0..pool_size).for_each(|_| v.push(Arc::new(RwLock::new(Page::new(0)))));
            v
        };
        let inner = Inner {
            pages,
            replacer,
            page_table: HashMap::with_capacity(pool_size),
            free_list,
        };
        Ok(Self {
            inner: RwLock::new(inner),
            disk_manager,
            next_page_id: AtomicUsize::new(0),
            pool_size,
        })
    }

    pub async fn new_page_ref(&self) -> RustDBResult<Option<PageRef>> {
        let mut inner = self.inner.write().await;
        println!("new page ref get inner");
        if let Some(frame_id) = self.available_frame(&mut inner).await? {
            let page_id = self.allocate_page();
            let page = Page::new(page_id);
            page.pin_count.store(1, Ordering::Relaxed);
            inner.pages[frame_id] = Arc::new(RwLock::new(page));
            inner.page_table.insert(page_id, frame_id);
            let mut replacer = inner.replacer.write().await;
            replacer.record_access(frame_id);
            replacer.set_evictable(frame_id, false);
            println!("new page ref drop inner");
            return Ok(inner
                .pages
                .get(frame_id)
                .map(|page| PageRef::new(page.clone(), frame_id, inner.replacer.clone())));
        }
        println!("new page ref drop inner");
        Ok(None)
    }

    pub async fn fetch_page_ref(&self, page_id: PageId) -> RustDBResult<Option<PageRef>> {
        let mut inner = self.inner.write().await;
        println!("fetch page ref: {} get inner", page_id);
        // fetch page from cache
        if let Some(frame_id) = inner.page_table.get(&page_id).cloned() {
            let page = inner.pages[frame_id].clone();
            println!("fetch page ref {} try to get page", page_id);
            {
                let page = page.read().await;
                page.pin_count.fetch_add(1, Ordering::Relaxed);
            }
            println!("fetch page ref {} try to get replacer", page_id);

            let mut replacer = inner.replacer.write().await;
            println!("fetch page ref {} try get replacer", page_id);
            replacer.record_access(frame_id);
            replacer.set_evictable(frame_id, false);
            println!("fetch page ref: {} drop inner", page_id);

            return Ok(Some(PageRef::new(
                page.clone(),
                frame_id,
                inner.replacer.clone(),
            )));
        }
        // fetch page from disk
        let frame_id = self.available_frame(&mut inner).await?;
        if let Some(frame_id) = frame_id {
            let page = inner.pages[frame_id].clone();
            {
                let mut page = page.write().await;
                self.disk_manager
                    .read_page(page_id, page.mut_data())
                    .await?;
                page.set_page_id(page_id);
                page.pin_count.store(1, Ordering::Relaxed);
            }
            inner.page_table.insert(page_id, frame_id);
            println!("fetch page ref {} try to get replacer", page_id);
            let mut replacer = inner.replacer.write().await;
            println!("fetch page ref {} try get replacer", page_id);
            replacer.record_access(frame_id);
            replacer.set_evictable(frame_id, false);
            println!("fetch page ref: {} drop inner", page_id);
            return Ok(Some(PageRef::new(
                page.clone(),
                frame_id,
                inner.replacer.clone(),
            )));
        }
        println!("fetch page ref: {} drop inner", page_id);
        Ok(None)
    }

    pub async fn flush_page(&self, page_id: PageId) -> RustDBResult<()> {
        let inner = self.inner.write().await;
        println!("flush_page: {} get inner", page_id);
        if let Some(frame_id) = inner.page_table.get(&page_id).cloned() {
            let page = inner.pages[frame_id].clone();
            let page = page.write().await;
            if page.is_dirty() {
                self.disk_manager
                    .write_page(page.page_id(), page.data())
                    .await?;
                page.set_dirty(false);
            }
        }
        Ok(())
    }

    pub async fn flush_page_all(&self) -> RustDBResult<()> {
        let inner = self.inner.write().await;
        for page in inner.pages.iter() {
            let page = page.write().await;
            if page.is_dirty() {
                self.disk_manager
                    .write_page(page.page_id(), page.data())
                    .await?;
                page.set_dirty(false);
            }
        }
        Ok(())
    }

    pub async fn delete_page(&self, page_id: PageId) -> RustDBResult<Option<PageId>> {
        let mut inner = self.inner.write().await;
        if let Some(frame_id) = inner.page_table.get(&page_id).cloned() {
            let page = inner.pages[frame_id].clone();
            let mut page = page.write().await;
            if page.pin_count.load(Ordering::Relaxed) > 0 {
                return Ok(None);
            }
            if page.is_dirty() {
                self.disk_manager
                    .write_page(page.page_id(), page.data())
                    .await?;
                page.set_dirty(false);
            }
            page.reset();
            inner.replacer.write().await.remove(frame_id)?;
            inner.free_list.push_back(frame_id);
            inner.page_table.remove(&page_id);
            return Ok(Some(page_id));
        }
        Ok(None)
    }
    async fn available_frame(
        &self,
        inner: &mut RwLockWriteGuard<'_, Inner>,
    ) -> RustDBResult<Option<FrameId>> {
        if let Some(frame_id) = inner.free_list.pop_front() {
            return Ok(Some(frame_id));
        }
        let frame_id = inner.replacer.write().await.evict();
        if let Some(frame_id) = frame_id {
            let page = inner.pages[frame_id].clone();
            let page = page.write().await;
            if page.is_dirty() {
                self.disk_manager
                    .write_page(page.page_id(), page.data())
                    .await?;
                page.set_dirty(false);
            }
            inner.page_table.remove(&page.page_id());
            return Ok(Some(frame_id));
        }
        Ok(None)
    }
    fn allocate_page(&self) -> PageId {
        self.next_page_id.fetch_add(1, Ordering::AcqRel)
    }
}

impl BufferPoolManager {
    pub async fn fetch_page_read_owned(&self, page_id: PageId) -> RustDBResult<OwnedPageReadGuard> {
        let page = self
            .fetch_page_ref(page_id)
            .await?
            .ok_or(RustDBError::BufferPool("Can't fetch page".into()))?
            .read_owned()
            .await;
        Ok(page)
    }

    pub async fn fetch_page_write_owned(
        &self,
        page_id: PageId,
    ) -> RustDBResult<OwnedPageWriteGuard> {
        let page = self
            .fetch_page_ref(page_id)
            .await?
            .ok_or(RustDBError::BufferPool("Can't fetch page".into()))?
            .write_owned()
            .await;
        Ok(page)
    }

    pub async fn new_page_write_owned<K>(
        &self,
        node: &mut Node<K>,
    ) -> RustDBResult<OwnedPageWriteGuard>
    where
        K: Encoder<Error = RustDBError>,
    {
        let guard = self
            .new_page_ref()
            .await?
            .ok_or(RustDBError::BufferPool("Can't new page".into()))?
            .write_owned()
            .await;
        let page_id = guard.page_id();
        node.set_page_id(page_id);
        Ok(guard)
    }
    pub async fn fetch_page_node<K>(&self, page_id: PageId) -> RustDBResult<(PageRef, Node<K>)>
    where
        K: Decoder<Error = RustDBError>,
    {
        let page = self
            .fetch_page_ref(page_id)
            .await?
            .ok_or(RustDBError::BufferPool("Can't fetch page".into()))?;
        let node = page.read().await.node()?;
        Ok((page, node))
    }

    pub async fn new_page_node<K>(&self, node: &mut Node<K>) -> RustDBResult<PageRef>
    where
        K: Encoder<Error = RustDBError>,
    {
        let page = self
            .new_page_ref()
            .await?
            .ok_or(RustDBError::BufferPool("Can't new page".into()))?;
        let page_id = page.read().await.page_id();
        node.set_page_id(page_id);
        Ok(page)
    }
}

pub struct PageRef {
    page: Arc<RwLock<Page>>,
    frame_id: FrameId,
    replacer: Arc<RwLock<LruKReplacer>>,
}

pub struct PageWriteGuard<'a> {
    guard: RwLockWriteGuard<'a, Page>,
}

pub struct PageReadGuard<'a> {
    guard: RwLockReadGuard<'a, Page>,
}

pub struct OwnedPageWriteGuard {
    guard: OwnedRwLockWriteGuard<Page>,
    page_ref: PageRef,
}

pub struct OwnedPageReadGuard {
    guard: OwnedRwLockReadGuard<Page>,
    page_ref: PageRef,
}

//todo async drop
impl Drop for PageRef {
    fn drop(&mut self) {
        let page = self.page.clone();
        let frame_id = self.frame_id;
        let replacer = self.replacer.clone();
        tokio::spawn(async move {
            let page = page.read().await;
            let prev = page.pin_count.fetch_sub(1, Ordering::Relaxed);
            if prev == 1 {
                replacer.write().await.set_evictable(frame_id, true);
            }
        });
    }
}

impl Deref for PageWriteGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl DerefMut for PageWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.deref_mut()
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        self.set_dirty(true);
    }
}

impl Deref for PageReadGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl Deref for OwnedPageWriteGuard {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl DerefMut for OwnedPageWriteGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.deref_mut()
    }
}

impl Drop for OwnedPageWriteGuard {
    fn drop(&mut self) {
        self.set_dirty(true);
    }
}

impl Deref for OwnedPageReadGuard {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl PageRef {
    pub fn new(
        page: Arc<RwLock<Page>>,
        frame_id: FrameId,
        replacer: Arc<RwLock<LruKReplacer>>,
    ) -> Self {
        Self {
            page,
            frame_id,
            replacer,
        }
    }
    pub async fn write(&self) -> PageWriteGuard<'_> {
        let guard = self.page.write().await;
        PageWriteGuard { guard }
    }

    pub async fn read(&self) -> PageReadGuard<'_> {
        let guard = self.page.read().await;
        PageReadGuard { guard }
    }

    pub async fn write_owned(self) -> OwnedPageWriteGuard {
        let guard = self.page.clone().write_owned().await;
        OwnedPageWriteGuard {
            guard,
            page_ref: self,
        }
    }

    pub async fn read_owned(self) -> OwnedPageReadGuard {
        let guard = self.page.clone().read_owned().await;
        OwnedPageReadGuard {
            guard,
            page_ref: self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::PAGE_SIZE;
    use std::io::Write;
    use std::time::Duration;

    #[tokio::test]
    async fn test_buffer_pool_manager() -> RustDBResult<()> {
        let random_data = [2u8; PAGE_SIZE];
        let db_name = "test_buffer_pool_manager.db";
        let buffer_pool_size = 10;
        let k = 5;
        // No matter if `char` is signed or unsigned by default, this constraint must be met

        let disk_manager = DiskManager::new(db_name).await?;
        let bpm = BufferPoolManager::new(buffer_pool_size, k, disk_manager).await?;

        let page0 = bpm.new_page_ref().await?;

        // Scenario: The buffer pool is empty. We should be able to create a new page.
        assert!(page0.is_some());
        let page0 = page0.unwrap();
        assert_eq!(0, page0.read().await.page_id());

        // Scenario: Once we have a page, we should be able to read and write content.
        page0
            .write()
            .await
            .mut_data()
            .clone_from_slice(&random_data);

        let mut pages = Vec::new();
        // Scenario: We should be able to create new pages until we fill up the buffer pool.
        for _ in 1..buffer_pool_size {
            let page = bpm.new_page_ref().await?;
            assert!(page.is_some());
            let page = page.unwrap();
            pages.push(page);
        }

        // Scenario: Once the buffer pool is full, we should not be able to create any new pages.
        for _i in buffer_pool_size..2 * buffer_pool_size {
            assert!(bpm.new_page_ref().await?.is_none())
        }

        // Scenario: After unpinning pages {0, 1, 2, 3, 4}, we should be able to create 5 new pages
        {
            let _page0 = page0.write().await;
        }
        drop(page0);
        for i in 0..4 {
            if let Some(page) = pages.get(i) {
                let _page = page.write().await;
            }
            let _page = pages.remove(0);
            bpm.flush_page(i).await?;
        }
        // wait until page unpin
        tokio::time::sleep(Duration::from_millis(100)).await;

        for _ in 0..5 {
            let page = bpm.new_page_ref().await?;
            assert!(page.is_some());
            let _page_id = page.unwrap().read().await.page_id();
        }
        // wait until page unpin
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Scenario: We should be able to fetch the data we wrote a while ago.
        let page0 = bpm.fetch_page_ref(0).await?;
        assert!(page0.is_some());
        let page0 = page0.unwrap();
        assert_eq!(page0.read().await.data(), &random_data);

        // Shutdown the disk manager and remove the temporary file we created.
        tokio::fs::remove_file(db_name).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_simple() -> RustDBResult<()> {
        let db_name = "test_simple.db";
        let buffer_pool_size = 10;
        let k = 5;

        let disk_manager = DiskManager::new(db_name).await?;
        let bpm = BufferPoolManager::new(buffer_pool_size, k, disk_manager).await?;

        let page0 = bpm.new_page_ref().await?;

        // Scenario: The buffer pool is empty. We should be able to create a new page.
        assert!(page0.is_some());
        let page0 = page0.unwrap();
        assert_eq!(0, page0.read().await.page_id());

        // Scenario: Once we have a page, we should be able to read and write content.
        let data = "Hello".as_bytes();
        page0.write().await.mut_data().write_all(data)?;

        // Scenario: We should be able to create new pages until we fill up the buffer pool.
        let mut pages = Vec::new();
        for _ in 1..buffer_pool_size {
            let page = bpm.new_page_ref().await?;
            assert!(page.is_some());
            let page = page.unwrap();
            {
                let _page = page.write().await;
            }
            pages.push(page);
        }

        // Scenario: Once the buffer pool is full, we should not be able to create any new pages.
        for _ in buffer_pool_size..buffer_pool_size * 2 {
            assert!(bpm.new_page_ref().await?.is_none());
        }

        // Scenario: After unpinning pages {0, 1, 2, 3, 4} and pinning another 4 new pages,
        // there would still be one buffer page left for reading page 0.
        {
            let _page0 = page0.write().await;
        }
        drop(page0);
        for _ in 0..4 {
            pages.remove(0);
        }
        // wait until page unpin
        tokio::time::sleep(Duration::from_millis(100)).await;

        for _ in 0..4 {
            let page = bpm.new_page_ref().await?;
            assert!(page.is_some());
            pages.push(page.unwrap());
        }

        // Scenario: We should be able to fetch the data we wrote a while ago.
        let page0 = bpm.fetch_page_ref(0).await?;
        assert!(page0.is_some());
        let page0 = page0.unwrap();
        let mut data = [0u8; PAGE_SIZE];
        let mut data_slice = &mut data[..];
        data_slice.write_all("Hello".as_bytes())?;
        assert_eq!(page0.read().await.data(), data);

        // Scenario: If we unpin page 0 and then make a new page, all the buffer pages should
        // now be pinned. Fetching page 0 again should fail.
        {
            let _page0 = page0.write().await;
        }
        drop(page0);
        // wait until page unpin
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(bpm.new_page_ref().await?.is_some());
        assert!(bpm.fetch_page_ref(0).await?.is_none());

        // Shutdown the disk manager and remove the temporary file we created.
        tokio::fs::remove_file(db_name).await?;

        Ok(())
    }
}
