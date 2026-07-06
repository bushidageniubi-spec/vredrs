use std::collections::HashMap;

/// 源文件管理器，用于存储所有加载的源文件内容
/// 供错误显示和调试使用
#[derive(Debug, Clone)]
pub struct SourceManager {
    /// 文件ID到源文件信息的映射
    files: HashMap<usize, SourceFile>,
}

/// 单个源文件的信息
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// 文件路径
    pub path: String,
    /// 文件内容
    pub content: String,
    /// 行起始位置索引（字节偏移）
    line_starts: Vec<usize>,
}

impl SourceFile {
    pub fn new(path: String, content: String) -> Self {
        let line_starts = Self::compute_line_starts(&content);
        SourceFile {
            path,
            content,
            line_starts,
        }
    }

    /// 计算每一行的起始字节偏移
    fn compute_line_starts(content: &str) -> Vec<usize> {
        let mut starts = vec![0];
        for (i, ch) in content.char_indices() {
            if ch == '\n' {
                starts.push(i + 1);
            }
        }
        starts
    }

    /// 获取指定行的内容（从1开始计数）
    pub fn get_line(&self, line_num: usize) -> Option<&str> {
        if line_num == 0 || line_num > self.line_starts.len() {
            return None;
        }
        let start = self.line_starts[line_num - 1];
        let end = if line_num < self.line_starts.len() {
            self.line_starts[line_num] - 1 // 去掉换行符
        } else {
            self.content.len()
        };

        if start <= end && end <= self.content.len() {
            Some(&self.content[start..end])
        } else {
            None
        }
    }

    /// 获取指定字节范围的内容
    pub fn get_slice(&self, start: usize, end: usize) -> Option<&str> {
        if start <= end && end <= self.content.len() {
            Some(&self.content[start..end])
        } else {
            None
        }
    }
}

impl SourceManager {
    pub fn new() -> Self {
        SourceManager {
            files: HashMap::new(),
        }
    }

    /// 加载源文件
    pub fn load_file(&mut self, file_id: usize, path: String, content: String) {
        self.files.insert(file_id, SourceFile::new(path, content));
    }

    /// 获取源文件信息
    pub fn get_file(&self, file_id: usize) -> Option<&SourceFile> {
        self.files.get(&file_id)
    }

    /// 获取指定行的内容
    pub fn get_line(&self, file_id: usize, line_num: usize) -> Option<&str> {
        self.get_file(file_id)?.get_line(line_num)
    }

    /// 获取文件路径
    pub fn get_path(&self, file_id: usize) -> Option<&str> {
        self.get_file(file_id).map(|f| f.path.as_str())
    }
}

impl Default for SourceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_starts() {
        let source = "line1\nline2\nline3";
        let file = SourceFile::new("test.vrs".into(), source.into());
        assert_eq!(file.line_starts, vec![0, 6, 12]);
    }

    #[test]
    fn test_get_line() {
        let source = "line1\nline2\nline3";
        let file = SourceFile::new("test.vrs".into(), source.into());
        assert_eq!(file.get_line(1), Some("line1"));
        assert_eq!(file.get_line(2), Some("line2"));
        assert_eq!(file.get_line(3), Some("line3"));
        assert_eq!(file.get_line(4), None);
        assert_eq!(file.get_line(0), None);
    }

    #[test]
    fn test_source_manager() {
        let mut mgr = SourceManager::new();
        mgr.load_file(0, "main.vrs".into(), "set, x = 10\npaste, x".into());

        assert_eq!(mgr.get_path(0), Some("main.vrs"));
        assert_eq!(mgr.get_line(0, 1), Some("set, x = 10"));
        assert_eq!(mgr.get_line(0, 2), Some("paste, x"));
        assert_eq!(mgr.get_line(0, 3), None);
    }
}
