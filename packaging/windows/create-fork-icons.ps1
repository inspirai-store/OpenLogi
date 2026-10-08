# Original geometric artwork for this fork. No upstream design assets are used.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$taskBitmap = [System.Drawing.Bitmap]::new(256, 256)
$taskGraphics = [System.Drawing.Graphics]::FromImage($taskBitmap)
$taskGraphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$taskGraphics.Clear([System.Drawing.Color]::FromArgb(255, 25, 41, 57))
$taskPen = [System.Drawing.Pen]::new([System.Drawing.Color]::FromArgb(255, 76, 214, 180), 24)
$taskGraphics.DrawArc($taskPen, 54, 54, 148, 148, 30, 300)
$taskGraphics.DrawLine($taskPen, 128, 128, 200, 128)
$taskGraphics.DrawLine($taskPen, 200, 128, 200, 172)
$taskBitmap.Save((Join-Path $PSScriptRoot 'fork-icon.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$taskStream = [System.IO.MemoryStream]::new()
$taskBitmap.Save($taskStream, [System.Drawing.Imaging.ImageFormat]::Png)
$taskBytes = $taskStream.ToArray()
$taskFile = [System.IO.File]::Create((Join-Path $PSScriptRoot 'fork-icon.ico'))
$taskWriter = [System.IO.BinaryWriter]::new($taskFile)
$taskWriter.Write([uint16]0); $taskWriter.Write([uint16]1); $taskWriter.Write([uint16]1)
$taskWriter.Write([byte]0); $taskWriter.Write([byte]0); $taskWriter.Write([byte]0); $taskWriter.Write([byte]0)
$taskWriter.Write([uint16]1); $taskWriter.Write([uint16]32)
$taskWriter.Write([uint32]$taskBytes.Length); $taskWriter.Write([uint32]22)
$taskWriter.Write($taskBytes)
$taskWriter.Dispose(); $taskStream.Dispose(); $taskPen.Dispose(); $taskGraphics.Dispose(); $taskBitmap.Dispose()
