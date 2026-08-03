# Disclaimer

## No Warranty

This software is provided "AS IS", without warranty of any kind, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose, and non-infringement. The entire risk as to the quality and performance of the software is with you.

## Limitation of Liability

In no event shall the authors or copyright holders be liable for any claim, damages, or other liability, whether in an action of contract, tort, or otherwise, arising from, out of, or in connection with the software or the use or other dealings in the software. This includes, without limitation, any direct, indirect, incidental, special, exemplary, or consequential damages (including but not limited to loss of data, loss of profits, or business interruption).

## Precompiled Binaries

Precompiled binaries published on the [releases page](https://github.com/tylern91/rqmd/releases) are provided solely for convenience and are covered by the same license as the source code (MIT). They are provided without warranties or conditions of any kind. You are responsible for verifying the integrity and suitability of any binary before use. Release tags are GPG-signed — verify the tag signature, or the checksum published alongside each binary, before running it.

## Third-Party Dependencies

This software incorporates third-party open-source components, each governed by their respective licenses, and downloads pretrained models from Hugging Face at runtime (see Data and Network Access below). The authors make no representations or warranties regarding these dependencies or downloaded models, and accept no liability for any issues arising from their use.

## Use at Your Own Risk

This software interacts with your file system, indexes the content you point it at, and — if you enable the MCP HTTP server — can expose that indexed content over the network with no built-in authentication (see [SECURITY.md](SECURITY.md)). It is your responsibility to ensure that its use, and how you configure it, is appropriate for your environment and complies with any applicable policies, regulations, or agreements. The authors are not responsible for any unintended side effects resulting from its use.

## Data and Network Access — no telemetry

rqmd collects and transmits **no usage metrics, telemetry, or analytics of any kind**. There is no opt-out setting because there is nothing to opt out of.

The software does make two categories of outbound network request, both for functionality rather than data collection:

- **Model downloads.** On first use, rqmd downloads pretrained embedding, reranking, and generation models from Hugging Face Hub. Set `HF_ENDPOINT` to use a mirror, or `HF_HUB_OFFLINE=1` to disable downloads entirely once models are pre-staged in the local cache.
- **Build-time ONNX Runtime fetch.** Building with the optional `ort-backend` feature downloads the ONNX Runtime library at build time.

No indexed document content, file paths, queries, or command arguments ever leave your machine as a result of using rqmd.

---

See [LICENSE](LICENSE) for the full terms of the MIT License under which this software is distributed.
