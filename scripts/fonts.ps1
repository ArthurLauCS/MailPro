# 下载界面用的两套开源中文字体到 crates/app/assets/fonts（不进 git，约 32 MB）。
# 开发运行和打安装包前各跑一次；文件已在且哈希对得上就跳过。许可证：思源宋体 SIL OFL 1.1，霞鹜新晰黑 IPA Font License 1.0。
$dir = "$PSScriptRoot\..\crates\app\assets\fonts"
New-Item -ItemType Directory -Force $dir | Out-Null
$fonts = @(
    # 标题和名字：思源宋体（可变字重）
    @{ Name = 'NotoSerifSC-VF.ttf'; Url = 'https://github.com/notofonts/noto-cjk/raw/Serif2.003/Serif/Variable/TTF/Subset/NotoSerifSC-VF.ttf'; Sha256 = '5326cfb097e3ab26fcb39329752b5c0a439bf8d5c4649520e4b492939c352a09' },
    # 邮件正文：霞鹜新晰黑，也是正文查看器的默认字体
    @{ Name = 'LXGWNeoXiHei.ttf'; Url = 'https://github.com/lxgw/LxgwNeoXiHei/releases/download/v1.308/LXGWNeoXiHei.ttf'; Sha256 = '80415d3666d9f51cf42ecf82c5350488e6b3f282ab46a3b90dcaa5e212564ecc' }
)
foreach ($f in $fonts) {
    $path = Join-Path $dir $f.Name
    if ((Test-Path $path) -and (Get-FileHash $path -Algorithm SHA256).Hash -eq $f.Sha256) { continue }
    Invoke-WebRequest $f.Url -OutFile $path -UseBasicParsing
    $hash = (Get-FileHash $path -Algorithm SHA256).Hash
    if ($hash -ne $f.Sha256) { throw "$($f.Name) SHA-256 mismatch: $hash" }
}
