//! Generate offline documentation from the same clap command as --help.
use crate::cli::Cli;
use crate::presentation::Language;

pub fn render(language: Language) -> Vec<u8> {
    let command = Cli::localized_command(language).color(clap::ColorChoice::Never);
    let mut page = Vec::new();
    clap_mangen::Man::new(command)
        .title("ZTAIL")
        .section("1")
        .date("")
        .source(concat!("ztail ", env!("CARGO_PKG_VERSION")))
        .manual("User Commands")
        .render(&mut page)
        .expect("manual fits in memory");
    page.extend_from_slice(language.text(EXTRA_EN, EXTRA_ZH).as_bytes());
    if matches!(language, Language::Chinese) {
        // Roff does not infer word boundaries between Han characters. Supply
        // invisible break opportunities and disable Latin-style justification.
        return cjk_layout(std::str::from_utf8(&page).expect("manual is UTF-8")).into_bytes();
    }
    page
}

fn cjk_layout(input: &str) -> String {
    let han = |ch: char| matches!(ch, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}');
    let mut output = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            output.push(ch);
            if han(ch) && chars.peek().is_some_and(|&next| han(next)) {
                output.push_str(r"\:");
            }
        }
        // man macros can restore adjustment, so reset after each such macro.
        if [".TH ", ".SH ", ".SS ", ".PP", ".TP"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            output.push_str(".na\n");
        }
    }
    output
}

const EXTRA_EN: &str = r#"
.SH "EXIT STATUS"
0: success, including a closed downstream pipe.
.br
1: file, format, or output error; or missing file operands.
.br
2: invalid command-line arguments.
.br
130: interrupted by SIGINT while following.
.br
143: terminated by SIGTERM while following.
.SH "FORMATS AND LIMITS"
Formats are detected by content, not filename. Gzip and plain text are always
available; zstd requires a build with the zstd feature. Xz is unsupported.
Only regular files are accepted: stdin, FIFOs, sockets, and devices are not read.
Without follow, an unterminated trailing line is not printed. Records exceeding
1 MiB are emitted as deterministic segments to bound pending-line memory.
.SH "ENVIRONMENT"
LC_ALL, LC_MESSAGES, LANG: first nonempty variable chooses the UI locale.
Use --language to override. Unknown locales fall back to English.
.br
NO_COLOR, CLICOLOR_FORCE, CLICOLOR, TERM affect automatic color. Explicit
--color always or never overrides environment settings. Stdout and stderr
are detected independently. Generated scripts and this manual are never colored.
.SH "SAFETY"
File names in headers and application diagnostics are escaped. Log payloads
are emitted unchanged and may contain terminal control sequences. Color control
is not sanitization. A large --lines value can retain substantial data in memory.
Follow mode is incremental, not an atomic snapshot of concurrently rewritten files.
.SH "SEE ALSO"
tail(1), gzip(1), zstd(1)
"#;

const EXTRA_ZH: &str = r#"
.SH "退出状态"
0：成功，包括下游管道已关闭。
.br
1：文件、格式或输出错误，或缺少文件参数。
.br
2：命令行参数无效。
.br
130：跟随时收到 SIGINT；143：跟随时收到 SIGTERM。
.SH "格式与限制"
按内容而非扩展名检测格式。始终支持 gzip 和纯文本；zstd 需要对应构建特性。
不支持 xz。仅接受普通文件，不读取标准输入、FIFO、套接字和设备。
非跟随模式不输出未以换行符结束的尾行。超过 1 MiB 的记录会分段输出以限制内存。
.SH "环境变量"
LC_ALL、LC_MESSAGES、LANG 中第一个非空值决定界面语言；--language 可覆盖。
未知语言回退到英文。NO_COLOR、CLICOLOR_FORCE、CLICOLOR、TERM 影响自动颜色；
显式 --color always 或 never 优先。分别检测标准输出与标准错误。
生成的脚本和手册不会添加颜色。
.SH "安全边界"
标题及应用诊断中的文件名已转义；日志正文保持原样，可能包含终端控制序列。
关闭颜色不等于净化日志。较大的 --lines 参数可能占用大量内存。
增量跟随不能保证并发重写文件的原子快照。
.SH "参见"
tail(1), gzip(1), zstd(1)
"#;
