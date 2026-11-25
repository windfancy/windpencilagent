//! Registry：组件的统一登记处与配置驱动组合器。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use thiserror::Error;

use crate::component::{Component, ComponentKind, ComponentSpec};

/// Registry 相关的错误。
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("component `{0}` already registered as {1}")]
    Duplicate(String, ComponentKind),
    #[error("no component of kind {kind} named `{name}`")]
    NotFound { kind: ComponentKind, name: String },
    #[error("component `{0}` not compatible with type {1}")]
    Incompatible(String, &'static str),
}

/// 装配阶段错误（从配置驱动组合时产生）。
#[derive(Debug, Error)]
pub enum AssemblyError {
    #[error("assembly failed: {0}")]
    Generic(String),
}

/// 组件注册表。按 `(kind, name)` 键控。
///
/// 组件以多态 `Arc<dyn Component>` 登记，支持统一展示与生命周期；
/// 具体类型的解析通过 `get_as`（依赖组件实现 `as_any`）完成。
#[derive(Default)]
pub struct Registry {
    components: RwLock<IndexMap<(ComponentKind, String), Arc<dyn Component>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 以类型安全方式注册组件。等价于 `register(Arc<T> as Arc<dyn Component>)`。
    pub fn register_typed<T: Component>(&self, comp: Arc<T>) -> Result<(), RegistryError> {
        self.register(comp as Arc<dyn Component>)
    }

    /// 注册一个类型擦除的组件（不提供 typed 视图；无法再取回具体类型）。
    #[allow(clippy::needless_lifetimes)]
    pub fn register(&self, comp: Arc<dyn Component>) -> Result<(), RegistryError> {
        let key = (comp.kind(), comp.id().to_string());
        let mut comps = self.components.write().unwrap();
        if comps.contains_key(&key) {
            return Err(RegistryError::Duplicate(comp.id().to_string(), comp.kind()));
        }
        comps.insert(key.clone(), comp);
        Ok(())
    }

    /// 按 kind 与名称取回组件（类型擦除视图）。
    pub fn get(&self, kind: ComponentKind, name: &str) -> Option<Arc<dyn Component>> {
        self.components.read().unwrap().get(&(kind, name.to_string())).cloned()
    }

    /// 是否存在指定组件。
    pub fn has(&self, kind: ComponentKind, name: &str) -> bool {
        self.components.read().unwrap().contains_key(&(kind, name.to_string()))
    }

    /// 列出某类组件的全部规格。
    pub fn list(&self, kind: ComponentKind) -> Vec<ComponentSpec> {
        self.components
            .read()
            .unwrap()
            .iter()
            .filter(|((k, _), _)| *k == kind)
            .map(|(_, c)| c.spec())
            .collect()
    }

    /// 列出全部组件规格。
    pub fn list_all(&self) -> Vec<ComponentSpec> {
        self.components.read().unwrap().values().map(|c| c.spec()).collect()
    }

    /// 移除指定组件（卸载流程的一部分）。
    pub fn unregister(&self, kind: ComponentKind, name: &str) -> bool {
        let key = (kind, name.to_string());
        self.components.write().unwrap().shift_remove(&key).is_some()
    }

    /// 统计：各类组件的数量。
    pub fn counts(&self) -> HashMap<String, usize> {
        let mut m: HashMap<String, usize> = HashMap::new();
        for (kind, _) in self.components.read().unwrap().keys() {
            *m.entry(kind.to_string()).or_insert(0) += 1;
        }
        m
    }

    pub fn len(&self) -> usize {
        self.components.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::ComponentStatus;
    

    struct DummyTool;

    impl Component for DummyTool {
        fn id(&self) -> &str {
            "dummy.tool"
        }
        fn kind(&self) -> ComponentKind {
            ComponentKind::Tool
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> String {
            "dummy".into()
        }
        fn status(&self) -> ComponentStatus {
            ComponentStatus::Loaded
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn register_and_get() {
        let r = Registry::new();
        r.register(Arc::new(DummyTool)).unwrap();
        assert!(r.has(ComponentKind::Tool, "dummy.tool"));
        assert_eq!(r.list_all().len(), 1);
        assert!(r.register(Arc::new(DummyTool)).is_err());
        r.unregister(ComponentKind::Tool, "dummy.tool");
        assert!(!r.has(ComponentKind::Tool, "dummy.tool"));
    }

    #[test]
    fn typed_register_and_get() {
        let r = Registry::new();
        r.register_typed::<DummyTool>(Arc::new(DummyTool)).unwrap();
        let got = r.get(ComponentKind::Tool, "dummy.tool");
        assert!(got.is_some());
        assert_eq!(r.counts().get("tool"), Some(&1));
    }
}
